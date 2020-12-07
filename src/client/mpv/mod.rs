use async_std::prelude::*;
use async_std::sync::Mutex;
use async_std::{io::BufReader, task};
use futures::{
    channel::{mpsc::UnboundedSender as Sender, oneshot},
    future,
};
use futures::{FutureExt, SinkExt};
use std::{
    collections::{HashMap, HashSet},
    fmt,
    time::{Duration, Instant},
};
use tracing::debug;
use tracing::info;
use tracing::trace;

use anyhow::{anyhow, bail, Context, Result};
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
    // List of expected events that are to be ignored.
    // (Usually because they were caused by ipc commands.)
    events_to_be_ignored: EventBundle,

    // request_ids of expected responses.
    expected_responses: HashSet<u64>,

    // Events that still require the current time to be send.
    required_time: Vec<Box<dyn FnOnce(f64) -> MpvEvent + Send + 'static>>,

    // Senders for request_time method.
    time_requests: Vec<oneshot::Sender<f64>>,

    // Senders for execute_event method responses.
    execution_requests: HashMap<u64, oneshot::Sender<ExecutionResponse>>,

    next_request_id: u64,
}

impl Default for MpvState {
    fn default() -> Self {
        Self {
            events_to_be_ignored: EventBundle::default(),
            expected_responses: HashSet::new(),
            required_time: Vec::new(),
            time_requests: Vec::new(),
            execution_requests: HashMap::new(),
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

impl MpvEvent {
    pub fn new_paused(pause: bool, time: f64) -> Self {
        if pause {
            Self::Pause { time }
        } else {
            Self::Resume { time }
        }
    }
}

#[must_use]
#[derive(Debug, Clone, Copy)]
pub enum ExecutionResponse {
    Success,
    Failed,
}

impl ExecutionResponse {
    fn and(self, other: Self) -> Self {
        match (self, other) {
            (Self::Success, Self::Success) => Self::Success,
            (Self::Failed, _) | (_, Self::Failed) => Self::Failed,
        }
    }
}

impl From<MpvIpcErrorStatus> for ExecutionResponse {
    fn from(e: MpvIpcErrorStatus) -> Self {
        match e {
            MpvIpcErrorStatus::Success => Self::Success,
            MpvIpcErrorStatus::InvalidParameter => Self::Failed,
            MpvIpcErrorStatus::PropertyUnavailable => Self::Failed,
        }
    }
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

        {
            let mut state = this.state.lock().await;
            state
                .events_to_be_ignored
                .push(MpvIpcEvent::PropertyChange {
                    id: OBSERVE_SPEED_ID,
                    name: MpvIpcPropertyValue::Speed(1.0),
                });
        }

        let _ = this.send_mpv_ipc_command(observe_speed_payload).await?;

        let observe_pause_payload = MpvIpcCommand::ObserveProperty {
            request_id: this.new_request_id().await,
            id: OBSERVE_PAUSE_ID,
            property: MpvIpcProperty::Pause,
        };

        {
            let mut state = this.state.lock().await;
            state
                .events_to_be_ignored
                .push(MpvIpcEvent::PropertyChange {
                    id: OBSERVE_PAUSE_ID,
                    name: MpvIpcPropertyValue::Pause(false),
                });
        }

        let _ = this.send_mpv_ipc_command(observe_pause_payload).await?;

        Ok(this)
    }

    async fn send_time_request(&self) -> Result<()> {
        let payload = MpvIpcCommand::GetProperty {
            request_id: TIMER_REQUEST_ID,
            property: MpvIpcProperty::TimePos,
        };
        let _ = self.send_mpv_ipc_command(payload).await?;
        Ok(())
    }

    async fn send_mpv_ipc_command(
        &self,
        cmd: MpvIpcCommand,
    ) -> Result<oneshot::Receiver<ExecutionResponse>> {
        let mut payload = serde_json::to_string(&cmd.to_json_command()).unwrap();
        trace!("sending: {:?}", payload);
        payload.push('\n');

        let request_id = cmd.get_request_id();

        let (sender, receiver) = oneshot::channel();

        {
            let mut state = self.state.lock().await;
            state.execution_requests.insert(request_id, sender);
            if request_id != TIMER_REQUEST_ID {
                state.expected_responses.insert(request_id);
            }
        }

        (&self.stream)
            .write_all(payload.as_bytes())
            .await
            .context("MPV socket failed.")?;

        Ok(receiver)
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
                        id: OBSERVE_PAUSE_ID,
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
                        drop(state);
                        self.send_time_request().await?;
                    }

                    MpvIpcEvent::PropertyChange {
                        id: OBSERVE_SPEED_ID,
                        name: MpvIpcPropertyValue::Speed(factor),
                    } => {
                        let event = MpvEvent::SpeedChange { factor };
                        sender.send(event).await?;
                    }

                    MpvIpcEvent::Seek => {
                        state
                            .required_time
                            .push(Box::new(|time| MpvEvent::Seek { time }));
                        drop(state);
                        self.send_time_request().await?;
                    }
                    _ => {}
                }
            }

            MpvIpcResponseOrEvent::Response(response) => {
                let request_id = response.request_id;

                if request_id == TIMER_REQUEST_ID {
                    let time = response.data.as_ref().unwrap().as_f64().unwrap();

                    for event in state.required_time.drain(..) {
                        sender.send(event(time)).await?;
                    }

                    for send in state.time_requests.drain(..) {
                        let _ = send.send(time);
                    }
                } else if state.expected_responses.remove(&request_id) {
                    let requested = state.execution_requests.remove(&request_id);

                    if let Some(send) = requested {
                        let _ = send.send(response.error.into());
                    } else {
                        info!("Ignored {:?}", response);
                    }

                    if response.error != MpvIpcErrorStatus::Success {
                        info!("Error MPV Response: {:?}", response);
                    }
                } else {
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

    /// Repeatedly tries to call try_execute_event until it succeeds
    /// If it fails all attempts it will error.
    pub async fn execute_event(&self, event: MpvEvent, attempts: u32) -> Result<()> {
        let mut attempt = 1;

        while let ExecutionResponse::Failed = self.try_execute_event(event).await? {
            trace!("event {:?} execution failed ({}).", event, attempt);

            task::sleep(Duration::from_millis(250)).await;

            if attempt > attempts {
                bail!(
                    "Unsuccessfully tried to execute {:?} for {} times.",
                    event,
                    attempts
                );
            }

            attempt += 1;
        }
        Ok(())
    }

    /// Requires the `event_loop` to be polled in order to terminate.
    pub async fn try_execute_event(
        &self,
        event: MpvEvent,
    ) -> Result<ExecutionResponse> {
        match event {
            MpvEvent::Pause { time } | MpvEvent::Resume { time } => {
                let request_id = self.new_request_id().await;

                let pause = matches!(event, MpvEvent::Pause { .. });

                let payload = MpvIpcCommand::SetProperty {
                    request_id,
                    property: MpvIpcPropertyValue::Pause(pause),
                };

                let observe_pause_event = MpvIpcEvent::PropertyChange {
                    id: OBSERVE_PAUSE_ID,
                    name: MpvIpcPropertyValue::Pause(pause),
                };

                {
                    let mut state = self.state.lock().await;
                    state.events_to_be_ignored.push(observe_pause_event);
                }

                let pause_response = self.send_mpv_ipc_command(payload).await?;
                let pause_response = pause_response
                    .map(|e| e.expect("pause execution sender dropped."));

                let seek_response = self.execute_seek(time).await?;

                let (r1, r2) = future::join(pause_response, seek_response).await;

                Ok(r1.and(r2))
            }
            MpvEvent::Seek { time } => {
                let response = self.execute_seek(time).await?;
                Ok(response.await)
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

                {
                    let mut state = self.state.lock().await;
                    state.events_to_be_ignored.push(ipc_event);
                }

                let response = self.send_mpv_ipc_command(payload).await?;

                let response =
                    response.await.expect("speed response channel closed.");

                Ok(response)
            }
        }
    }

    async fn new_request_id(&self) -> u64 {
        let mut state = self.state.lock().await;
        let id = state.next_request_id;
        state.next_request_id += 1;
        id
    }

    async fn execute_seek(
        &self,
        time: f64,
    ) -> Result<impl Future<Output = ExecutionResponse>> {
        let payload = {
            let mut state = self.state.lock().await;
            let request_id = state.next_request_id;
            state.next_request_id += 1;

            state.events_to_be_ignored.push(MpvIpcEvent::Seek);

            MpvIpcCommand::SetProperty {
                request_id,
                property: MpvIpcPropertyValue::TimePos(time),
            }
        };

        let response = self.send_mpv_ipc_command(payload).await?;

        let response = response.map(|r| {
            r.expect("Channel closed before receiving execution response.")
        });

        Ok(response)
    }

    pub async fn request_time(&self) -> Result<f64> {
        let (time_sender, time_receiver) = oneshot::channel();

        {
            let mut state = self.state.lock().await;
            state.time_requests.push(time_sender);
        }

        self.send_time_request().await?;

        let time = time_receiver
            .await
            .expect("Channel closed before receiving time.");

        Ok(time)
    }
}
