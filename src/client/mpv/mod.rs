use async_std::io::BufReader;
use async_std::prelude::*;
use async_std::sync::Mutex;
use futures::channel::{mpsc::{UnboundedSender as Sender}, oneshot};
use futures::SinkExt;
use std::{collections::HashSet, fmt, time::{Duration, Instant}};
use tracing::debug;
use tracing::info;
use tracing::trace;

use anyhow::{anyhow, Context, Result};
use async_std::os::unix::net::UnixStream;

mod ipc_data_model;

use ipc_data_model::*;

pub struct Mpv {
    stream: UnixStream,
    state: Mutex<MpvState>,
}

const TIMER_REQUEST_ID: u64 = 3;
const OBSERVE_PAUSE_ID: u64 = 1;
const OBSERVE_SPEED_ID: u64 = 2;

#[derive(Default)]
struct EventBundle(Vec<(MpvIpcEvent, Instant)>);

impl EventBundle {
    fn push(&mut self, event: MpvIpcEvent) {
        self.0.push((event, Instant::now()));
    }

    fn remove_if_contains(&mut self, event: &MpvIpcEvent) -> bool {
        // Remove old events, which might've been forgotten by mpv.
        self.0
            .retain(|(_, since)| since.elapsed() < Duration::from_millis(500));

        if let Some((idx, _)) =
            self.0.iter().enumerate().find(|(_, (e, _))| e == event)
        {
            self.0.swap_remove(idx);
            true
        } else {
            false
        }
    }
}

impl fmt::Debug for EventBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut f = f.debug_list();

        for (event, time) in self.0.iter() {
            f.entry(&(event, time.elapsed().as_secs_f32()));
        }

        f.finish()
    }
}

struct MpvState {
    events_to_be_ignored: EventBundle,
    expected_responses: HashSet<u64>,
    required_time: Vec<Box<dyn FnOnce(f64) -> MpvEvent + Send + 'static>>,
    time_requests: Vec<oneshot::Sender<f64>>,
    next_request_id: u64,
}

impl Default for MpvState {
    fn default() -> Self {
        Self {
            events_to_be_ignored: EventBundle::default(),
            expected_responses: HashSet::new(),
            required_time: Vec::new(),
            time_requests: Vec::new(),
            next_request_id: TIMER_REQUEST_ID + 1,
        }
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MpvEvent {
    Pause { time: f64 },
    Resume { time: f64 },
    Seek { time: f64 },
    SpeedChange { factor: f64 },
}

impl Mpv {
    pub async fn new(stream: UnixStream) -> Result<Self> {
        let this = Self {
            stream,
            state: Mutex::new(MpvState::default()),
        };

        let observe_speed_payload = MpvIpcCommand::ObserveProperty {
            request_id: this.new_request_id().await,
            id: OBSERVE_SPEED_ID,
            property: MpvIpcProperty::Speed,
        };

        let mut state = this.state.lock().await;
        state
            .events_to_be_ignored
            .push(MpvIpcEvent::PropertyChange {
                id: OBSERVE_SPEED_ID,
                name: MpvIpcPropertyValue::Speed(1.0),
            });
        drop(state);

        this.send_mpv_ipc_command(observe_speed_payload).await?;

        let observe_pause_payload = MpvIpcCommand::ObserveProperty {
            request_id: this.new_request_id().await,
            id: OBSERVE_PAUSE_ID,
            property: MpvIpcProperty::Pause,
        };

        let mut state = this.state.lock().await;
        state
            .events_to_be_ignored
            .push(MpvIpcEvent::PropertyChange {
                id: OBSERVE_PAUSE_ID,
                name: MpvIpcPropertyValue::Pause(false),
            });
        drop(state);

        this.send_mpv_ipc_command(observe_pause_payload).await?;

        Ok(this)
    }

    async fn send_time_request(&self) -> Result<()> {
        let payload = MpvIpcCommand::GetProperty {
            request_id: TIMER_REQUEST_ID,
            property: MpvIpcProperty::TimePos,
        };
        self.send_mpv_ipc_command(payload).await
    }

    async fn send_mpv_ipc_command(&self, cmd: MpvIpcCommand) -> Result<()> {
        trace!("sending: {:?}", cmd);

        let mut payload = serde_json::to_string(&cmd.to_json_command()).unwrap();
        payload.push('\n');

        if cmd.get_request_id() != TIMER_REQUEST_ID {
            let mut state = self.state.lock().await;
            state.expected_responses.insert(cmd.get_request_id());
            drop(state);
        }

        (&self.stream)
            .write_all(payload.as_bytes())
            .await
            .context("MPV socket failed.")?;

        Ok(())
    }

    async fn process_mpv_response(
        &self,
        msg: MpvIpcResponseOrEvent,
        sender: &mut Sender<MpvEvent>,
    ) -> Result<()> {
        let mut state = self.state.lock().await;
        match msg {
            MpvIpcResponseOrEvent::Event(event) => {
                trace!("Events bundle: {:?}", state.events_to_be_ignored);
                if state.events_to_be_ignored.remove_if_contains(&event) {
                    trace!("Ignored event: {:?}", event);
                    return Ok(());
                }

                match event {
                    MpvIpcEvent::PropertyChange {
                        id: 1,
                        name: MpvIpcPropertyValue::Pause(paused),
                    } => {
                        if paused {
                            state
                                .required_time
                                .push(Box::new(|time| MpvEvent::Pause { time }));
                        } else {
                            state
                                .required_time
                                .push(Box::new(|time| MpvEvent::Resume { time }));
                        }
                        self.send_time_request().await?;
                    }

                    MpvIpcEvent::PropertyChange {
                        id: 2,
                        name: MpvIpcPropertyValue::Speed(factor),
                    } => {
                        let event = MpvEvent::SpeedChange { factor };
                        sender.send(event).await?;
                    }

                    MpvIpcEvent::Seek => {
                        state
                            .required_time
                            .push(Box::new(|time| MpvEvent::Seek { time }));
                        self.send_time_request().await?;
                    }
                    _ => {}
                }
            }

            MpvIpcResponseOrEvent::Response(response) => {
                if response.request_id == TIMER_REQUEST_ID {
                    let time = response.data.as_ref().unwrap().as_f64().unwrap();

                    for event in state.required_time.drain(..) {
                        sender.send(event(time)).await?;
                    }

                    for send in state.time_requests.drain(..) {
                        let _ = send.send(time);
                    }
                } else if !state.expected_responses.remove(&response.request_id) {
                    info!("Unexpected Response: {:?}", response);
                }
            }
        }

        Ok(())
    }

    pub async fn event_loop(&self, mut sender: Sender<MpvEvent>) -> Result<()> {
        let mut stream = BufReader::new(&self.stream);
        let mut line = String::new();

        loop {
            line.clear();
            let bytes_read = stream
                .read_line(&mut line)
                .await
                .context("Connection to mpv ipc socket lost.")?;

            if bytes_read == 0 {
                Err(anyhow!("Read 0 bytes."))
                    .context("Connection to mpv ipc socket lost.")?;
            }

            trace!("receiving: {}", line.trim());

            let res = serde_json::from_str(&line);

            match res {
                Ok(msg) => self.process_mpv_response(msg, &mut sender).await?,
                Err(_) => debug!("Couldn't parse mpv response: {}", line.trim()),
            }
        }
    }

    pub async fn execute_event(&self, event: MpvEvent) -> Result<()> {
        match event {
            MpvEvent::Pause { time } | MpvEvent::Resume { time } => {
                let request_id = self.new_request_id().await;

                let pause = matches!(event, MpvEvent::Pause { .. });

                let payload = MpvIpcCommand::SetProperty {
                    request_id,
                    property: MpvIpcPropertyValue::Pause(pause),
                };

                self.send_mpv_ipc_command(payload).await?;

                let observe_pause_event = MpvIpcEvent::PropertyChange {
                    id: OBSERVE_PAUSE_ID,
                    name: MpvIpcPropertyValue::Pause(pause),
                };

                let mut state = self.state.lock().await;
                state.events_to_be_ignored.push(observe_pause_event);
                drop(state);

                self.execute_seek(time).await?;
            }
            MpvEvent::Seek { time } => {
                self.execute_seek(time).await?;
            }
            MpvEvent::SpeedChange { factor } => {
                let request_id = self.new_request_id().await;
                let payload = MpvIpcCommand::SetProperty {
                    request_id,
                    property: MpvIpcPropertyValue::Speed(factor),
                };

                let ipc_event = MpvIpcEvent::PropertyChange {
                    id: OBSERVE_SPEED_ID,
                    name: MpvIpcPropertyValue::Speed(factor),
                };

                let mut state = self.state.lock().await;
                state.events_to_be_ignored.push(ipc_event);
                drop(state);

                self.send_mpv_ipc_command(payload).await?;
            }
        }
        Ok(())
    }

    async fn new_request_id(&self) -> u64 {
        let mut state = self.state.lock().await;
        let id = state.next_request_id;
        state.next_request_id += 1;
        drop(state);
        id
    }

    pub async fn execute_seek(&self, time: f64) -> Result<()> {
        let mut state = self.state.lock().await;

        let request_id = state.next_request_id;
        state.next_request_id += 1;

        let payload = MpvIpcCommand::SetProperty {
            request_id,
            property: MpvIpcPropertyValue::TimePos(time),
        };

        state.events_to_be_ignored.push(MpvIpcEvent::Seek);

        drop(state);

        self.send_mpv_ipc_command(payload).await?;
        Ok(())
    }

    pub async fn request_time(&self) -> Result<f64> {
        let (time_sender, time_receiver) = oneshot::channel();

        let mut state = self.state.lock().await;
        state.time_requests.push(time_sender);
        drop(state);

        self.send_time_request().await?;

        time_receiver
            .await
            .map_err(|_| anyhow!("Channel closed before receiving time."))
    }
}
