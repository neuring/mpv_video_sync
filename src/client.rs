use crossbeam::channel::Sender;
use crossbeam::channel::{self, Receiver};
use crossbeam::select;
use crossbeam::thread;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Read;
use std::io::Write;
use std::mem::Discriminant;
use std::net::TcpStream;
use std::os::unix::net::UnixStream;
use std::time::Duration;

type Result<T> = std::result::Result<T, anyhow::Error>;

mod mpv_data_model;
mod network_data_model;

use mpv_data_model::*;
use network_data_model::*;

fn mpv_socket_loop<S: Read>(
    mpv_events: Sender<MpvResponseOrEvent>,
    socket: S,
) -> Result<()> {
    let reader = BufReader::new(socket);

    println!("Listening to mpv ...");
    for line in reader.lines() {
        let line = line?;

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
        mpv_events.send(response)?;
    }

    Ok(())
}

fn network_loop(
    network_events: Sender<ClientMessage>,
    network_socket: &TcpStream,
) -> Result<()> {
    let network = BufReader::new(network_socket);

    println!("Listening to network...");
    for line in network.lines() {
        let line = line?;

        println!("{: <10}--> {:?}", "NETWORK", line);

        let msg = serde_json::from_str(&line)?;

        network_events.send(msg)?;
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

fn request_time_pos(mut mpv_stream: impl Write) -> Result<()> {
    let cmd = MpvCommand::GetProperty {
        property: MpvProperty::TimePos,
        request_id: TIMER_REQUEST_ID,
    };
    mpv_stream.write_all(cmd.to_json_command().to_string().as_bytes())?;
    mpv_stream.write(b"\n")?;
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

fn broker_handle_mpv_event(
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

            serde_json::to_writer(network_stream, &payload)?;
            network_stream.write(b"\n")?;
        }

        MpvResponseOrEvent::Event(MpvEvent::PropertyChange {
            id: _,
            name: MpvPropertyValue::Speed(factor),
        }) => {
            let payload = ClientMessage::SpeedChange { factor };

            serde_json::to_writer(network_stream, &payload)?;
            network_stream.write(b"\n")?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Pause) => {
            if let Some(Command::Pause) = response_state.last_event { return Ok(()) }
            response_state.last_event = Some(Command::Pause);

            response_state.command_waiting_for_time = Some(Command::Pause);
            request_time_pos(mpv_stream)?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Unpause) => {
            if let Some(Command::Resume) = response_state.last_event { return Ok(()) }
            response_state.last_event = Some(Command::Resume);

            response_state.command_waiting_for_time = Some(Command::Resume);
            request_time_pos(mpv_stream)?;
        }

        MpvResponseOrEvent::Event(MpvEvent::Seek) => {
            if let Some(discriminant) = response_state.last_network_command {
                if discriminant == std::mem::discriminant(&ClientMessage::Seek{ time: 0.0 }) {
                    response_state.last_network_command = None;
                    return Ok(());
                }
            }
            response_state.command_waiting_for_time = Some(Command::Seek);
            request_time_pos(mpv_stream)?;
        }

        _ => {
            println!("Unhandled event: {:?}", event);
        }
    };
    Ok(())
}

fn broker_handle_network_event(
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
        serde_json::to_writer(mpv_stream, &action.to_json_command())?;
        mpv_stream.write(b"\n")?;
    }

    if let Some(time) = time {
        let time_payload = MpvCommand::SetProperty {
            request_id: 1,
            property: MpvPropertyValue::TimePos(time),
        };
        serde_json::to_writer(mpv_stream, &time_payload.to_json_command())?;
        mpv_stream.write(b"\n")?;
    }

    Ok(())
}

fn broker_loop(
    mpv_events: Receiver<MpvResponseOrEvent>,
    network_events: Receiver<ClientMessage>,
    mut mpv_stream: &UnixStream,
    mut network_stream: &TcpStream,
) -> Result<()> {
    let mut response_state = BrokerResponseState::default();

    loop {
        select! {
            recv(mpv_events) -> event => {
                let event = event?;
                broker_handle_mpv_event(event, &mut mpv_stream, &mut network_stream, &mut response_state)?;
            },
            recv(network_events) -> event => {
                let event = event?;
                broker_handle_network_event(event, &mut mpv_stream, &mut response_state)?;
            }
            default(Duration::from_millis(2000)) => {
                request_time_pos(&mut mpv_stream)?;
            }
        };
    }
}

fn run() -> Result<()> {
    let ipc_socket_path = std::env::args().nth(1).unwrap();
    let mut ipc_stream = UnixStream::connect(ipc_socket_path)?;
    println!("Connected to mpv socket!");

    let speed_change = MpvCommand::ObserveProperty {
        request_id: SPEED_CHANGE_REQUEST_ID,
        id: 1,
        property: MpvProperty::Speed,
    };
    serde_json::to_writer(&ipc_stream, &speed_change.to_json_command())?;
    ipc_stream.write(b"\n")?;

    let network_path = std::env::args().nth(2).unwrap();
    let network_stream = TcpStream::connect(network_path)?;
    println!("Connected to network!");

    thread::scope(|s| -> Result<()> {
        let (mpv_sender, mpv_receiver) = channel::unbounded();
        let (network_sender, network_receiver) = channel::unbounded();

        let mpv_listener_handle =
            s.spawn(|_| mpv_socket_loop(mpv_sender, &ipc_stream));

        let network_listener_handle =
            s.spawn(|_| network_loop(network_sender, &network_stream));

        let broker_handle = s.spawn(|_| {
            broker_loop(mpv_receiver, network_receiver, &ipc_stream, &network_stream)
        });

        mpv_listener_handle.join().unwrap()?;
        network_listener_handle.join().unwrap()?;
        broker_handle.join().unwrap()?;
        Ok(())
    })
    .unwrap()?;

    Ok(())
}

fn main() {
    run().unwrap();
}
