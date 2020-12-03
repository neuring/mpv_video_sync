use anyhow::Context;
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
use tracing::trace;
use std::io::ErrorKind;
use std::sync::Arc;
use std::time::Duration;
use tracing::debug;
use tracing::debug_span;
use tracing_futures::Instrument;

use anyhow::{anyhow, Result};

use crate::mpv::{Mpv, MpvEvent};

use video_sync::*;

use crate::Config;

async fn network_listen_loop(
    mut network_events: Sender<ServerMessage>,
    network_socket: &TcpStream,
) -> Result<()> {
    let mut network = BufReader::new(network_socket);
    let mut line = String::new();

    debug!("Listening to network...");
    loop {
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

        let msg = serde_json::from_str(&line)
            .context("Synchronization server broke protocol.")?;

        trace!("receiving: {:?}", msg);

        network_events.send(msg).await?;
    }
}

async fn broker_handle_network_event(event: ServerMessage, mpv: &Mpv) -> Result<()> {
    match event {
        ServerMessage::Update {
            time, paused, speed, 
        } => {
            match (time, paused) {
                (Some(time), Some(true)) => {
                    mpv.execute_event(MpvEvent::Pause { time }).await?;
                },
                (Some(time), Some(false)) => {
                    mpv.execute_event(MpvEvent::Resume { time }).await?;
                },
                (Some(time), None) => {
                    mpv.execute_event(MpvEvent::Seek { time }).await?;
                },
                _ => {},
            }

            if let Some(factor) = speed {
                mpv.execute_event(MpvEvent::SpeedChange { factor }).await?;
            }
        },
    }
    Ok(())
}

async fn send_network_message(
    msg: ClientMessage,
    mut network: &TcpStream,
) -> Result<()> {
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
            send_network_message(ClientMessage::Pause { time }, network).await?
        }
        MpvEvent::Resume { time } => {
            send_network_message(ClientMessage::Resume { time }, network).await?
        }
        MpvEvent::Seek { time } => {
            send_network_message(ClientMessage::Seek { time }, network).await?
        }
        MpvEvent::SpeedChange { factor } => {
            send_network_message(ClientMessage::SpeedChange { factor }, network)
                .await?
        }
    }
    Ok(())
}

async fn broker_loop(
    mpv_events: Receiver<MpvEvent>,
    network_events: Receiver<ServerMessage>,
    mpv: &Mpv,
    network_stream: &TcpStream,
) -> Result<()> {
    let mut mpv_events = mpv_events
        .fuse();
    let mut network_events = network_events
        .fuse();
    let mut has_received_initial_server_message = false;

    loop {
        select! {
            mpv_event = mpv_events.next().fuse() => {
                if let Some(event) = mpv_event {
                    debug!("MPV EVENT: {:?}", mpv_event);
                    if has_received_initial_server_message {
                        broker_handle_mpv_event(
                            event,
                            network_stream,
                        ).await?;
                    }
                }
            },

            network_event = network_events.next().fuse() => {
                if let Some(event) = network_event {
                    debug!("NETWORK EVENT: {:?}", network_event);
                    broker_handle_network_event(event, mpv).await?;
                    has_received_initial_server_message = true;
                }
            },

            _ = task::sleep(Duration::from_millis(2000)).fuse() => {
                if has_received_initial_server_message {
                    trace!("Requesting time");
                    let time = mpv.request_time().await?;
                    let msg = ClientMessage::Timestamp { time };
                    send_network_message(msg, network_stream).await?;
                }
            },
        }
    }
}

async fn try_ipc_connection(config: &Config) -> Result<UnixStream> {
    let mut attempts = 0;
    let mut last_err = anyhow!("");
    trace!("Start connecting to {}", &config.ipc_socket);
    let ipc_socket = loop {
        let stream = UnixStream::connect(&config.ipc_socket).await;

        match stream {
            Ok(stream) => break stream,
            Err(e) => match e.kind() {
                ErrorKind::NotFound | ErrorKind::ConnectionRefused => {
                    trace!("IPC socket connection attempt ({}) failed: {}", attempts + 1, e);
                    last_err = e.into();
                }
                _ => Err(e)?,
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

    let (mpv_sender, mpv_receiver) = mpsc::unbounded();
    let (network_sender, network_receiver) = mpsc::unbounded();

    let mut mpv_listener = mpv
        .event_loop(mpv_sender)
        .instrument(debug_span!("MPV event loop"))
        .boxed()
        .fuse();
    let mut network_listener = network_listen_loop(network_sender, &network_stream)
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
