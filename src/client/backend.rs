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
use std::mem::Discriminant;
use std::sync::Arc;
use std::time::Duration;

type Result<T> = std::result::Result<T, anyhow::Error>;

use crate::mpv_data_model::*;
use video_sync::*;

use crate::Config;

async fn mpv_socket_loop(
    mut mpv_events: Sender<MpvResponseOrEvent>,
    socket: &UnixStream,
) -> Result<()> {
    let reader = BufReader::new(socket);
    let mut lines = reader.lines();

    println!("Listening to mpv ...");
    while let Some(line) = lines.next().await {
        let line: String = line?;

        let response: MpvResponseOrEvent = match serde_json::from_str(&line.trim()) {
            Ok(response) => response,
            Err(error) => {
                eprintln!("Couldn't parse mpv response: {:?}", error);
                continue;
            }
        };

        match &response {
            MpvResponseOrEvent::Event(event) => {
                println!("{: <10}--> {:?}", "MPV-EVENT", event);
            }
            MpvResponseOrEvent::Response(response) => {
                println!("{: <10}--> {:?}", "MPV-RESP", response);
            }
        };
        mpv_events.send(response).await?;
    }

    Ok(())
}

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

const TIMER_REQUEST_ID: u64 = 42;
const SPEED_CHANGE_REQUEST_ID: u64 = 43;

#[derive(Debug)]
enum Command {
    Pause,
    Resume,
    Seek,
}

async fn request_time_pos(mut mpv_stream: &UnixStream) -> Result<()> {
    let cmd = MpvCommand::GetProperty {
        property: MpvProperty::TimePos,
        request_id: TIMER_REQUEST_ID,
    };
    println!("{: <10}<-- {:?}", "MPV", cmd);
    mpv_stream
        .write_all(cmd.to_json_command().to_string().as_bytes())
        .await?;
    mpv_stream.write(b"\n").await?;
    Ok(())
}

#[derive(Debug, Default)]
struct BrokerResponseState {
    command_waiting_for_time: Option<Command>,
    last_event: Option<Command>, // MPV fires the Pause and Unpause events twice.
    // We avoid sending these messages twice over the
    // network by filtering with help of this field.
    last_network_command: Option<Discriminant<ClientMessage>>,
}

async fn broker_handle_mpv_event(
    event: MpvResponseOrEvent,
    mpv_stream: &UnixStream,
    mut network_stream: &TcpStream,
    response_state: &mut BrokerResponseState,
) -> Result<()> {
    match event {
        MpvResponseOrEvent::Response(MpvResponse {
            request_id: TIMER_REQUEST_ID,
            error,
            data,
        }) => {
            if error != MpvErrorStatus::Success {
                println!("Unexpected timer response error: {:?}", error);
            }

            let time = data
                .expect("MPV returns data if no error.")
                .as_f64()
                .expect("MPV returns f64.");

            let payload = match response_state.command_waiting_for_time {
                Some(Command::Pause) => ClientMessage::Pause { time },
                Some(Command::Resume) => ClientMessage::Resume { time },
                Some(Command::Seek) => ClientMessage::Seek { time },
                None => ClientMessage::Timestamp { time },
            };

            response_state.command_waiting_for_time = None;

            println!("{: <10}<-- {:?}", "NETWORK", payload);

            let payload = serde_json::to_string(&payload)?;
            network_stream.write_all(payload.as_bytes()).await?;
            network_stream.write(b"\n").await?;
        }

        MpvResponseOrEvent::Event(MpvEvent::PropertyChange {
            id: _,
            name: MpvPropertyValue::Speed(factor),
        }) => {
            let payload = ClientMessage::SpeedChange { factor };
            println!("{: <10}<-- {:?}", "NETWORK", payload);

            let payload = serde_json::to_string(&payload)?;
            network_stream.write_all(payload.as_bytes()).await?;
            network_stream.write(b"\n").await?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Pause) => {
            if let Some(Command::Pause) = response_state.last_event {
                return Ok(());
            }
            response_state.last_event = Some(Command::Pause);

            response_state.command_waiting_for_time = Some(Command::Pause);
            request_time_pos(mpv_stream).await?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Unpause) => {
            if let Some(Command::Resume) = response_state.last_event {
                return Ok(());
            }
            response_state.last_event = Some(Command::Resume);

            response_state.command_waiting_for_time = Some(Command::Resume);
            request_time_pos(mpv_stream).await?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Seek) => {
            if let Some(discriminant) = response_state.last_network_command {
                if discriminant
                    == std::mem::discriminant(&ClientMessage::Seek { time: 0.0 })
                {
                    response_state.last_network_command = None;
                    return Ok(());
                }
            }
            response_state.command_waiting_for_time = Some(Command::Seek);
            request_time_pos(mpv_stream).await?;
        }

        _ => {
            println!("Unhandled event: {:?}", event);
        }
    };
    Ok(())
}

async fn broker_handle_network_event(
    event: ClientMessage,
    mut mpv_stream: &UnixStream,
    response_state: &mut BrokerResponseState,
) -> Result<()> {
    let (action, time) = match event {
        ClientMessage::Pause { time } => {
            let pause_payload = MpvCommand::SetProperty {
                request_id: 1,
                property: MpvPropertyValue::Pause(true),
            };
            (Some(pause_payload), Some(time))
        }
        ClientMessage::Resume { time } => {
            let pause_payload = MpvCommand::SetProperty {
                request_id: 1,
                property: MpvPropertyValue::Pause(false),
            };
            (Some(pause_payload), Some(time))
        }
        ClientMessage::Seek { time } => (None, Some(time)),
        ClientMessage::SpeedChange { factor } => {
            let pause_payload = MpvCommand::SetProperty {
                request_id: 1,
                property: MpvPropertyValue::Speed(factor),
            };
            (Some(pause_payload), None)
        }
        msg @ _ => {
            eprintln!("Received unexpected message: {:?}", msg);
            return Ok(());
        }
    };
    response_state.last_network_command = Some(std::mem::discriminant(&event));

    if let Some(action) = action {
        println!("{: <10}<-- {:?}", "MPV", action);
        let payload = serde_json::to_string(&action.to_json_command())?;
        mpv_stream.write_all(payload.as_bytes()).await?;
        mpv_stream.write(b"\n").await?;
    }

    if let Some(time) = time {
        let time_payload = MpvCommand::SetProperty {
            request_id: 1,
            property: MpvPropertyValue::TimePos(time),
        };
        println!("{: <10}<-- {:?}", "MPV", time_payload);
        let payload = serde_json::to_string(&time_payload.to_json_command())?;
        mpv_stream.write_all(payload.as_bytes()).await?;
        mpv_stream.write(b"\n").await?;
    }

    Ok(())
}

async fn broker_loop(
    mpv_events: Receiver<MpvResponseOrEvent>,
    network_events: Receiver<ClientMessage>,
    mpv_stream: &UnixStream,
    network_stream: &TcpStream,
) -> Result<()> {
    let mut response_state = BrokerResponseState::default();
    let mut mpv_events = mpv_events.fuse();
    let mut network_events = network_events.fuse();

    loop {
        select! {
            mpv_event = mpv_events.next().fuse() => {
                if let Some(event) = mpv_event {
                    broker_handle_mpv_event(
                        event,
                        mpv_stream,
                        network_stream,
                        &mut response_state
                    ).await?;
                }
            },

            network_event = network_events.next().fuse() => {
                if let Some(event) = network_event {
                    broker_handle_network_event(event, mpv_stream, &mut response_state).await?;
                }
            },

            _ = task::sleep(Duration::from_millis(2000)).fuse() => {
                request_time_pos(mpv_stream).await?;
            },
        }
    }
}

pub async fn start_backend(config: Arc<Config>) -> Result<()> {
    let mut ipc_stream = loop {
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

    println!("Connected to mpv socket!");

    let speed_change = MpvCommand::ObserveProperty {
        request_id: SPEED_CHANGE_REQUEST_ID,
        id: 1,
        property: MpvProperty::Speed,
    };

    let payload = serde_json::to_string(&speed_change.to_json_command()).unwrap();
    ipc_stream.write_all(payload.as_bytes()).await?;
    ipc_stream.write(b"\n").await?;

    let network_stream = TcpStream::connect(&config.network_url).await.unwrap();
    println!("Connected to network!");

    let (mpv_sender, mpv_receiver) = mpsc::unbounded();
    let (network_sender, network_receiver) = mpsc::unbounded();

    let mut mpv_listener = mpv_socket_loop(mpv_sender, &ipc_stream).boxed().fuse();
    let mut network_listener =
        network_loop(network_sender, &network_stream).boxed().fuse();
    let mut broker =
        broker_loop(mpv_receiver, network_receiver, &ipc_stream, &network_stream)
            .boxed()
            .fuse();

    select! {
        x = mpv_listener => x?,
        x = network_listener => x?,
        x = broker => x?,
    }

    Ok(())
}
