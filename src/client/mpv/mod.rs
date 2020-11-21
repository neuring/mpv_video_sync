use async_std::io::BufReader;
use async_std::prelude::*;
use async_std::sync::Mutex;
use futures::channel::mpsc::UnboundedSender as Sender;
use futures::SinkExt;
use std::collections::HashSet;

use anyhow::Result;
use async_std::os::unix::net::UnixStream;

mod ipc_data_model;

use ipc_data_model::*;

pub struct Mpv {
    stream: UnixStream,
    state: Mutex<MpvState>,
}

const TIMER_REQUEST_ID: u64 = 3;
const SPEED_CHANGE_REQUEST_ID: u64 = 2;
const PAUSE_CHANGE_REQUEST_ID: u64 = 1;

struct MpvState {
    events_to_be_consumed: Vec<MpvIpcEvent>,
    expected_responses: HashSet<u64>,
    required_time: Vec<Box<dyn FnOnce(f64) -> MpvEvent + Send + 'static >>,
    next_request_id: u64,
}

impl Default for MpvState {
    fn default() -> Self {
        Self {
            events_to_be_consumed: Vec::new(),
            expected_responses: HashSet::new(),
            required_time: Vec::new(),
            next_request_id: TIMER_REQUEST_ID + 1,
        }
    }
}

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
            request_id: SPEED_CHANGE_REQUEST_ID,
            id: 2,
            property: MpvIpcProperty::Speed,
        };

        this.send_mpv_ipc_command(observe_speed_payload).await?;

        let observe_pause_payload = MpvIpcCommand::ObserveProperty {
            request_id: PAUSE_CHANGE_REQUEST_ID,
            id: 1,
            property: MpvIpcProperty::Pause,
        };

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
        let mut payload = serde_json::to_string(&cmd.to_json_command()).unwrap();
        payload.push('\n');

        (&self.stream).write_all(payload.as_bytes()).await?;

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
                let idx = state
                    .events_to_be_consumed
                    .iter()
                    .enumerate()
                    .find(|(_, e)| e == &&event);
                if let Some((idx, _)) = idx {
                    state.events_to_be_consumed.swap_remove(idx);
                    return Ok(());
                }

                match event {
                    MpvIpcEvent::PropertyChange {
                        id: PAUSE_CHANGE_REQUEST_ID,
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
                        id: SPEED_CHANGE_REQUEST_ID,
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
                }

                if state.expected_responses.remove(&response.request_id) {
                    eprintln!("Unexpected Response: {:?}", response);
                }
            }
        }

        Ok(())
    }

    pub async fn event_loop(&self, mut sender: Sender<MpvEvent>) -> Result<()> {
        let stream = BufReader::new(&self.stream);
        let mut lines = stream.lines();

        while let Some(line) = lines.next().await {
            let line: String = line?;

            let res = serde_json::from_str(&line);

            match res {
                Ok(msg) => self.process_mpv_response(msg, &mut sender).await?,
                Err(_) => eprintln!("Couldn't parse response: {}", line),
            }
        }

        Ok(())
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
        state.expected_responses.insert(request_id);

        let payload = MpvIpcCommand::SetProperty {
            request_id,
            property: MpvIpcPropertyValue::TimePos(time),
        };

        state.events_to_be_consumed.push(MpvIpcEvent::Seek);

        drop(state);

        self.send_mpv_ipc_command(payload).await?;
        Ok(())
    }

    pub async fn request_time(&self) -> f64 {
        todo!()
    }
}