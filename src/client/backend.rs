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

type Result<T> = std::result::Result<T, anyhow::Error>;

use crate::mpv::{Mpv, MpvEvent};

use video_sync::*;

use crate::Config;

async fn network_loop(
    mut network_events: Sender<ClientMessage>,
    network_socket: &TcpStream,
) -> Result<()> {
    let network = BufReader::new(network_socket);
    let mut lines = network.lines();

    println!("Listening to network...");
    while let Some(line) = lines.next().await {
        let line: String = line?;

        println!("{: <10}--> {:?}", "NETWORK", line);

        let msg = serde_json::from_str(&line)?;

        network_events.send(msg).await?;
    }

    Ok(())
}

async fn broker_handle_network_event(
    event: ClientMessage,
    mpv: &Mpv,
) -> Result<()> {
    match event {
        ClientMessage::Pause { time } => {
            mpv.execute_event(MpvEvent::Pause { time }).await?;
        }
        ClientMessage::Resume { time } => {
            mpv.execute_event(MpvEvent::Resume { time }).await?;
        },
        ClientMessage::Seek { time } => {
            mpv.execute_event(MpvEvent::Seek { time }).await?;
        },
        ClientMessage::SpeedChange { factor } => {
            mpv.execute_event(MpvEvent::SpeedChange { factor }).await?;
        },
        msg @ _ => {
            eprintln!("Received unexpected message: {:?}", msg);
            return Ok(());
        }
    }
    Ok(())
}

async fn send_network_message(
    msg: ClientMessage,
    mut network: &TcpStream,
) -> Result<()> {
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
            send_network_message(ClientMessage::SpeedChange { factor }, network).await?
        }
    }
    Ok(())
}

async fn broker_loop(
    mpv_events: Receiver<MpvEvent>,
    network_events: Receiver<ClientMessage>,
    mpv: &Mpv,
    network_stream: &TcpStream,
) -> Result<()> {
    let mut mpv_events = mpv_events.fuse();
    let mut network_events = network_events.fuse();

    loop {
        select! {
            mpv_event = mpv_events.next().fuse() => {
                if let Some(event) = mpv_event {
                    broker_handle_mpv_event(
                        event,
                        network_stream,
                    ).await?;
                }
            },

            network_event = network_events.next().fuse() => {
                if let Some(event) = network_event {
                    broker_handle_network_event(event, mpv).await?;
                }
            },

            _ = task::sleep(Duration::from_millis(2000)).fuse() => {
                println!("Requesting time");
                let time = mpv.request_time().await?;
                let msg = ClientMessage::Timestamp { time };
                send_network_message(msg, network_stream).await?;
            },
        }
    }
}

pub async fn start_backend(config: Arc<Config>) -> Result<()> {
    let ipc_socket = loop {
        let stream = UnixStream::connect(&config.ipc_socket).await;

        match stream {
            Ok(stream) => break stream,
            Err(e) => match e.kind() {
                ErrorKind::ConnectionRefused => {}
                _ => Err(e)?,
            },
        }

        task::sleep(Duration::from_millis(100)).await;
    };

    let mpv = Mpv::new(ipc_socket).await?;

    println!("Connected to mpv socket!");

    let network_stream = TcpStream::connect(&config.network_url).await?;
    println!("Connected to network!");

    let (mpv_sender, mpv_receiver) = mpsc::unbounded();
    let (network_sender, network_receiver) = mpsc::unbounded();

    let mut mpv_listener = mpv.event_loop(mpv_sender).boxed().fuse();
    let mut network_listener =
        network_loop(network_sender, &network_stream).boxed().fuse();
    let mut broker =
        broker_loop(mpv_receiver, network_receiver, &mpv, &network_stream)
            .boxed()
            .fuse();

    select! {
        x = mpv_listener => x?,
        x = network_listener => x?,
        x = broker => x?,
    }

    Ok(())
}
