use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::io::BufRead;
use std::io::BufReader;
use std::io::Write;
use std::net::TcpListener;
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;

mod network_data_model;

use crossbeam::channel;
use crossbeam::channel::Receiver;
use crossbeam::channel::Sender;
use crossbeam::thread;
use network_data_model::*;

type Result<T> = std::result::Result<T, anyhow::Error>;

type Id = u64;

#[derive(Debug, Clone, Copy)]
struct Timestamp {
    value: f64,
    when: Instant,
}

impl Timestamp {
    fn now(value: f64) -> Self {
        Self {
            value,
            when: Instant::now(),
        }
    }
}

struct Connection {
    stream: Arc<TcpStream>,
    timestamp: Option<Timestamp>,
}

enum Command {
    NewConnection(Id, Arc<TcpStream>),
    Disconnection(Id),
    Message(Id, ClientMessage),
    Synchronize,
}

fn data_loop(commands: Receiver<Command>) -> Result<()> {
    let mut connections = HashMap::new();

    let mut process_command = |command| -> Result<()> {
        match command {
            Command::NewConnection(id, stream) => {
                if let Entry::Vacant(e) = connections.entry(id) {
                    e.insert(Connection {
                        stream,
                        timestamp: None,
                    });
                } else {
                    panic!("Existing connection with id: {}", id);
                }
            }
            Command::Disconnection(id) => {
                connections
                    .remove(&id)
                    .expect("ID doesn't exist in connections.");
            }
            Command::Message(id, msg) => match msg {
                ClientMessage::Timestamp { time } => {
                    connections.get_mut(&id).unwrap().timestamp =
                        Some(Timestamp::now(time));
                }
                ClientMessage::Seek { .. }
                | ClientMessage::Pause { .. }
                | ClientMessage::Resume { .. }
                | ClientMessage::SpeedChange { .. } => {
                    for (_, Connection { stream, .. }) in
                        connections.iter().filter(|&(&con_id, _)| con_id != id)
                    {
                        println!(
                            "Sending {:?} to {}",
                            msg,
                            stream.local_addr().unwrap()
                        );
                        serde_json::to_writer(&**stream, &msg)?;
                        (&**stream).write(b"\n")?;
                    }
                }
            },
            Command::Synchronize => {
                println!("Synchronizing...");
                let iter = connections.iter()
                    .filter_map(|(_, &Connection { timestamp, .. })| Some(timestamp?.value))
                    .filter(|v| !v.is_nan());
                let min = iter.clone().min_by(|a, b| a.partial_cmp(b).unwrap());
                let max = iter.max_by(|a, b| a.partial_cmp(b).unwrap());


                if let (Some(min), Some(max)) = (min, max) {
                    if max - min > 5. {
                        println!("Synchronizing to necessary ({}, {})!", min, max);
                        let payload = ClientMessage::Seek { time : min};
                        for (_, Connection {stream, ..} ) in connections.iter() {
                            serde_json::to_writer(&**stream, &payload)?;
                            (&**stream).write(b"\n")?;
                        }
                    }
                }
            }
        }
        Ok(())
    };

    for command in commands {
        if let Err(err) = process_command(command) {
            println!("error occured when processing command: {:?}", err);
        }
    }

    Ok(())
}

fn connection_handler(
    stream: TcpStream,
    id: Id,
    commands: Sender<Command>,
) -> Result<()> {
    let addr = stream.local_addr()?;
    println!("Connected with {}", addr);

    let stream = Arc::new(stream);
    commands.send(Command::NewConnection(id, Arc::clone(&stream)))?;

    let buf_stream = BufReader::new(&*stream);

    let handle_input = |line: std::result::Result<String, _>| -> Result<()> {
        let line = line?;
        println!("From {}: {}", addr, &line);

        let msg = serde_json::from_str(&line)?;
        commands.send(Command::Message(id, msg))?;

        Ok(())
    };

    for line in buf_stream.lines() {
        if let Err(e) = handle_input(line) {
            println!("Error from {}: {}", addr, e);
            break;
        }
    }

    println!("Disconnecting {}", addr);
    commands.send(Command::Disconnection(id))?;

    Ok(())
}

fn run() -> Result<()> {
    let addr = std::env::args().nth(1).unwrap();
    let tcp = TcpListener::bind(addr)?;
    let (command_sender, command_receiver) = channel::unbounded();

    thread::scope(|s| -> Result<()> {
        let data_handler = s.spawn(|_| data_loop(command_receiver));

        let synchronize_handler = s.spawn(|_| -> Result<()> {
            loop {
                std::thread::sleep(Duration::from_secs(3));
                if let Err(_) = command_sender.send(Command::Synchronize) {
                    return Ok(());
                }
            }
        });

        let mut next_id = 1;
        for connection in tcp.incoming() {
            let connection = connection?;

            let command_sender = &command_sender;
            let id = next_id;
            s.spawn(move |_| {
                connection_handler(connection, id, command_sender.clone())
            });

            next_id += 1;
        }

        synchronize_handler.join().unwrap()?;
        data_handler.join().unwrap()?;
        Ok(())
    })
    .unwrap()?;

    Ok(())
}

fn main() {
    run().unwrap();
}
