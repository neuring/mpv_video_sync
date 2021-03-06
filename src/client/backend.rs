use std::{io::ErrorKind, iter, sync::Arc, time::Duration};

use anyhow::{anyhow, bail, Context, Result};
use async_std::{
    io::BufReader, net::TcpStream, os::unix::net::UnixStream, prelude::*, task,
};
use futures::{
    channel::{
        mpsc,
        mpsc::{UnboundedReceiver as Receiver, UnboundedSender as Sender},
    },
    select, FutureExt, SinkExt,
};
use itertools::Itertools;
use tracing::{debug, debug_span, trace};
use tracing_futures::Instrument;
use video_sync::*;

use crate::{
    mpv::{Mpv, MpvEvent},
    notification::Notificator,
    Config,
};

struct NetworkListener<'stream> {
    stream: BufReader<&'stream TcpStream>,
    buffer: String,
    network_events: Sender<ServerMessage>,
}

impl<'a> NetworkListener<'a> {
    fn new(stream: &'a TcpStream, events: Sender<ServerMessage>) -> Self {
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

            self.network_events.send(msg).await?;
        }
    }
}

async fn broker_handle_network_event(
    event: ServerMessage,
    mpv: &Mpv,
    notificator: &Notificator,
) -> Result<()> {
    match event {
        ServerMessage::UserUpdate(update) => match update {
            UserUpdate::Connected(user) => {
                notificator.notify(format!("{} has connected!", user))
            }
            UserUpdate::Disconnected(user) => {
                notificator.notify(format!("{} has disconnected!", user))
            }
        },
        ServerMessage::PlayerUpdate(PlayerUpdate {
            state:
                PlayerState {
                    time,
                    paused,
                    speed,
                },
            cause,
        }) => {
            match (time, paused) {
                (Some(time), Some(true)) => {
                    if let UpdateCause::UserAction(user) = &cause {
                        notificator.notify(format!("{} paused!", user));
                    }

                    mpv.execute_event(MpvEvent::Pause { time }, 1).await?;
                }

                (Some(time), Some(false)) => {
                    if let UpdateCause::UserAction(user) = &cause {
                        notificator.notify(format!("{} resumed!", user));
                    }

                    mpv.execute_event(MpvEvent::Resume { time }, 1).await?;
                }

                (Some(time), None) => {
                    if let UpdateCause::Synchronize = &cause {
                        notificator.notify(format!("Synchronization to {}!", time));
                    } else if let UpdateCause::UserAction(user) = &cause {
                        notificator.notify(format!("{} seeked to {}!", user, time));
                    }

                    mpv.execute_event(MpvEvent::Seek { time }, 1).await?;
                }
                _ => {}
            }

            if let Some(factor) = speed {
                if let UpdateCause::UserAction(user) = &cause {
                    notificator
                        .notify(format!("{} changed speed to {}!", user, factor));
                }

                mpv.execute_event(MpvEvent::SpeedChange { factor }, 1)
                    .await?;
            }
        }
        ServerMessage::Disconnect(reason) => match reason {
            ServerDisconnect::IncorrectHash => bail!("Incorrect video file."),
        },
        ServerMessage::Init(_) => bail!("Server broke protocol by re-initialising."),
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
    network_events: Receiver<ServerMessage>,
    mpv: &Mpv,
    network_stream: &TcpStream,
    config: Arc<Config>,
) -> Result<()> {
    let mut mpv_events = mpv_events.fuse();
    let mut network_events = network_events.fuse();
    let notificator = Notificator::new(Arc::clone(&config));

    // First wait for initial server message to set up mpv player.
    let init_msg = network_events
        .next()
        .await
        .context("Didn't receive init server message")?;

    if let ServerMessage::Init(ServerInit {
        player_state:
            PlayerState {
                time: Some(time),
                paused: Some(paused),
                speed: Some(speed),
            },
        users,
    }) = init_msg
    {
        mpv.execute_event(MpvEvent::new_paused(paused, time), 10)
            .await?;
        mpv.execute_event(MpvEvent::SpeedChange { factor: speed }, 1)
            .await?;

        if users.is_empty() {
            notificator.notify("Connected to server, no other users connected.");
        } else {
            let users = users
                .iter()
                .map(|user| user.as_str())
                .interleave_shortest(iter::repeat(", "))
                .collect::<String>();
            notificator.notify(format!("Connected to server with {}", users));
        }
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
                    debug!("NETWORK EVENT: {:?}", event);
                    broker_handle_network_event(event, mpv, &notificator).await?;
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
            name: config.username.0.clone(),
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
    let mut broker = broker_loop(
        mpv_receiver,
        network_receiver,
        &mpv,
        &network_stream,
        Arc::clone(&config),
    )
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
