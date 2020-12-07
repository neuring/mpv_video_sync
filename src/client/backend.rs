use anyhow::Context;
use anyhow::{anyhow, bail, Result};
use async_std::io::BufReader;
use async_std::net::TcpStream;
use async_std::os::unix::net::UnixStream;
use async_std::prelude::*;
use async_std::task;
use futures::channel::mpsc;
use futures::channel::mpsc::UnboundedReceiver as Receiver;
use futures::channel::mpsc::UnboundedSender as Sender;
use futures::select;
use futures::FutureExt;
use futures::SinkExt;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;
use tracing::debug_span;
use tracing::trace;
use tracing_futures::Instrument;

use crate::mpv::{Mpv, MpvEvent};
use crate::Config;

use video_sync::*;

struct NetworkListener<'stream> {
    stream: BufReader<&'stream TcpStream>,
    buffer: String,
    network_events: Sender<ServerUpdate>,
}

impl<'a> NetworkListener<'a> {
    fn new(stream: &'a TcpStream, events: Sender<ServerUpdate>) -> Self {
        Self {
            stream: BufReader::new(stream),
            network_events: events,
            buffer: String::new(),
        }
    }

    async fn receive_next_message(&mut self) -> Result<ServerMessage> {
        let network = &mut self.stream;
        let mut line = &mut self.buffer;

        line.clear();
        let bytes_read = network
            .read_line(&mut line)
            .await
            .context("Reading from synchronization server failed.")?;

        // I can't figure out a better way to detect if connection was broken.
        if bytes_read == 0 {
            Err(anyhow!("Read 0 bytes."))
                .context("Connection to synchronization server lost.")?;
        }

        let msg = serde_json::from_str(&line).with_context(|| {
            format!("Synchronization server broke protocol: \"{}\"", line.trim())
        })?;

        trace!("receiving: {:?}", msg);

        Ok(msg)
    }

    async fn listen(&mut self) -> Result<()> {
        debug!("Listening to network...");
        loop {
            let msg = self.receive_next_message().await?;

            match msg {
                ServerMessage::Update(update) => {
                    self.network_events.send(update).await?
                }
                ServerMessage::Disconnect(ServerDisconnect::IncorrectHash) => {
                    bail!("Incorrect video file.")
                }
            }
        }
    }
}

async fn broker_handle_network_event(event: ServerUpdate, mpv: &Mpv) -> Result<()> {
    match event {
        ServerUpdate {
            time,
            paused,
            speed,
        } => {
            match (time, paused) {
                (Some(time), Some(true)) => {
                    mpv.execute_event(MpvEvent::Pause { time }, 1).await?;
                }
                (Some(time), Some(false)) => {
                    mpv.execute_event(MpvEvent::Resume { time }, 1).await?;
                }
                (Some(time), None) => {
                    mpv.execute_event(MpvEvent::Seek { time }, 1).await?;
                }
                _ => {}
            }

            if let Some(factor) = speed {
                mpv.execute_event(MpvEvent::SpeedChange { factor }, 1)
                    .await?;
            }
        }
    }
    Ok(())
}

async fn send_network_message(
    msg: impl Into<ClientMessage>,
    mut network: &TcpStream,
) -> Result<()> {
    let msg = msg.into();
    trace!("sending: {:?}", msg);
    let mut payload = serde_json::to_string(&msg).unwrap();
    payload.push('\n');
    network.write_all(payload.as_bytes()).await?;
    Ok(())
}

async fn broker_handle_mpv_event(
    event: MpvEvent,
    network: &TcpStream,
) -> Result<()> {
    match event {
        MpvEvent::Pause { time } => {
            send_network_message(ClientUpdate::Pause { time }, network).await?
        }
        MpvEvent::Resume { time } => {
            send_network_message(ClientUpdate::Resume { time }, network).await?
        }
        MpvEvent::Seek { time } => {
            send_network_message(ClientUpdate::Seek { time }, network).await?
        }
        MpvEvent::SpeedChange { factor } => {
            send_network_message(ClientUpdate::SpeedChange { factor }, network)
                .await?
        }
    }
    Ok(())
}

async fn broker_loop(
    mpv_events: Receiver<MpvEvent>,
    network_events: Receiver<ServerUpdate>,
    mpv: &Mpv,
    network_stream: &TcpStream,
) -> Result<()> {
    let mut mpv_events = mpv_events.fuse();
    let mut network_events = network_events.fuse();

    // First wait for initial server message to set up mpv player.
    let init_msg = network_events
        .next()
        .await
        .context("Didn't receive init server message")?;

    if let ServerUpdate {
        time: Some(time),
        paused: Some(paused),
        speed: Some(speed),
    } = init_msg
    {
        mpv.execute_event(MpvEvent::new_paused(paused, time), 10)
            .await?;
        mpv.execute_event(MpvEvent::SpeedChange { factor: speed }, 1)
            .await?;
    } else {
        bail!("Invalid initial server message: {:?}", init_msg);
    }

    loop {
        select! {
            mpv_event = mpv_events.next().fuse() => {
                if let Some(event) = mpv_event {
                    debug!("MPV EVENT: {:?}", mpv_event);
                    broker_handle_mpv_event(
                        event,
                        network_stream,
                    ).await?;
                }
            },

            network_event = network_events.next().fuse() => {
                if let Some(event) = network_event {
                    debug!("NETWORK EVENT: {:?}", network_event);
                    broker_handle_network_event(event, mpv).await?;
                }
            },

            _ = task::sleep(Duration::from_millis(2000)).fuse() => {
                trace!("Requesting time");
                let time = mpv.request_time().await?;
                let msg = ClientUpdate::Timestamp { time };
                send_network_message(msg, network_stream).await?;
            },
        }
    }
}

async fn try_ipc_connection(config: &Config) -> Result<UnixStream> {
    let mut attempts = 0;
    let mut last_err: anyhow::Error;
    trace!("Start connecting to {}", &config.ipc_socket);
    let ipc_socket = loop {
        let stream = UnixStream::connect(&config.ipc_socket).await;

        match stream {
            Ok(stream) => break stream,
            Err(e) => match e.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
                    trace!(
                        "IPC socket connection attempt ({}) failed: {}",
                        attempts + 1,
                        e
                    );
                    last_err = e.into();
                }
                _ => return Err(e.into()),
            },
        }

        task::sleep(Duration::from_millis(100)).await;

        attempts += 1;
        if attempts > 20 {
            return Err(last_err).context("Connection to mpv ipc socket failed.");
        }
    };
    Ok(ipc_socket)
}

pub async fn start_backend(config: Arc<Config>) -> Result<()> {
    let ipc_socket = try_ipc_connection(&*config).await?;
    let mpv = Mpv::new(ipc_socket).await?;
    debug!("Connected to mpv socket!");

    let network_stream = TcpStream::connect(&config.network_url)
        .await
        .with_context(|| {
            format!(
                "Connection to synchronization server ({}) failed.",
                &config.network_url
            )
        })?;

    debug!("Connected to network!");

    send_network_message(
        ClientInit {
            video_hash: config.video_hash.clone(),
        },
        &network_stream,
    )
    .await
    .context("Couldn't send init message.")?;

    let (mpv_sender, mpv_receiver) = mpsc::unbounded();
    let (network_sender, network_receiver) = mpsc::unbounded();

    let mut network_listener = NetworkListener::new(&network_stream, network_sender);

    let mut mpv_listener = mpv
        .event_loop(mpv_sender)
        .instrument(debug_span!("MPV event loop"))
        .boxed()
        .fuse();
    let mut network_listener = network_listener
        .listen()
        .instrument(debug_span!("Network listener loop"))
        .boxed()
        .fuse();
    let mut broker =
        broker_loop(mpv_receiver, network_receiver, &mpv, &network_stream)
            .instrument(debug_span!("Broker loop"))
            .boxed()
            .fuse();

    select! {
        x = mpv_listener => {
            debug!("mpv_listener finished.");
            x
        }
        x = network_listener => {
            debug!("network_listener finished.");
            x
        }
        x = broker => {
            debug!("broker finished.");
            x
        }
    }
}
