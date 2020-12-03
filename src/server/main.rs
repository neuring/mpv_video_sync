use anyhow::bail;
use anyhow::Context;
use async_std::io::prelude::BufReadExt;
use async_std::io::prelude::WriteExt;
use async_std::io::BufReader;
use async_std::net::TcpListener;
use async_std::net::TcpStream;
use async_std::task;
use futures::channel::mpsc::{
    self, UnboundedReceiver as Receiver, UnboundedSender as Sender,
};
use futures::SinkExt;
use futures::StreamExt;
use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant;
use tracing::info;
use tracing::info_span;
use tracing::Instrument;
use tracing::{debug, warn};
use tracing_subscriber::EnvFilter;

use structopt::StructOpt;

use anyhow::{anyhow, Result};

use video_sync::*;

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

/// Shared state over all connections.
#[derive(Default)]
struct GlobalState {
    connections: HashMap<Id, Connection>,
    player: PlayerState,
}

impl GlobalState {
    // Get the minimum time of all connections.
    // Returns None if there are no connections.
    fn get_min_time(&self) -> Option<f64> {
        self.connections
            .iter()
            .filter_map(|(_, c)| c.timestamp)
            .map(|t| t.value)
            .filter(|v| !v.is_nan())
            .min_by(|a, b| a.partial_cmp(b).unwrap())
    }

    // Get the maximum time of all connections.
    // Returns None if there are no connections.
    fn get_max_time(&self) -> Option<f64> {
        self.connections
            .iter()
            .filter_map(|(_, c)| c.timestamp)
            .map(|t| t.value)
            .filter(|v| !v.is_nan())
            .max_by(|a, b| a.partial_cmp(b).unwrap())
    }
}

/// Expected state of all client mpv players.
/// The time is ommited because it is kept for each client separately in `Connection`
struct PlayerState {
    speed: f64,
    paused: bool,
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            speed: 1.0,
            paused: false,
        }
    }
}

async fn send_message(mut stream: &TcpStream, msg: ServerMessage) -> Result<()> {
    let mut payload = serde_json::to_string(&msg).unwrap();
    payload.push('\n');

    stream
        .write_all(payload.as_bytes())
        .await
        .with_context(|| format!("Failed to send {:?}", msg))
}

async fn process_command(command: Command, state: &mut GlobalState) -> Result<()> {
    match command {
        Command::NewConnection(id, stream) => {
            let min_time = state.get_min_time().unwrap_or(0.0);
            if let Entry::Vacant(e) = state.connections.entry(id) {
                let connection = e.insert(Connection {
                    stream,
                    timestamp: None,
                });

                let msg = ServerMessage::new()
                    .with_time(min_time)
                    .with_speed(state.player.speed)
                    .with_pause(state.player.paused);

                send_message(connection.stream.as_ref(), msg).await?;
            } else {
                bail!("Existing connection with id: {}", id);
            }
        }
        Command::Disconnection(id) => {
            state.connections.remove(&id).with_context(|| {
                format!("ID: {} doesn't exist in connections.", id)
            })?;
        }
        Command::Message(id, msg) => match msg {
            ClientMessage::Timestamp { time } => {
                state.connections.get_mut(&id).unwrap().timestamp =
                    Some(Timestamp::now(time));
            }
            ClientMessage::Seek { .. }
            | ClientMessage::Pause { .. }
            | ClientMessage::Resume { .. }
            | ClientMessage::SpeedChange { .. } => {
                debug!("Received {:?} from id {}", msg, id);

                let payload = ServerMessage::new();

                let payload = match msg {
                    ClientMessage::Seek { time } => payload.with_time(time),
                    ClientMessage::Pause { time } => {
                        payload.with_time(time).with_pause(true)
                    }
                    ClientMessage::Resume { time } => {
                        payload.with_time(time).with_pause(false)
                    }
                    ClientMessage::SpeedChange { factor } => {
                        payload.with_time(factor)
                    }
                    _ => {
                        warn!("Unreachable match arm reached.");
                        return Ok(());
                    }
                };

                let mut payload = serde_json::to_string(&payload).unwrap();
                payload.push('\n');

                for (id, Connection { stream, .. }) in state
                    .connections
                    .iter()
                    .filter(|&(&con_id, _)| con_id != id)
                {
                    debug!(
                        "Sending {:?} to {} ({})",
                        msg,
                        stream.peer_addr().unwrap(),
                        id
                    );

                    (&**stream).write_all(payload.as_bytes()).await?;
                }

                match msg {
                    ClientMessage::Pause { .. } => state.player.paused = true,
                    ClientMessage::Resume { .. } => state.player.paused = false,
                    ClientMessage::SpeedChange { factor } => {
                        state.player.speed = factor
                    }
                    _ => {}
                }
            }
        },
        Command::Synchronize => {
            let min = state.get_min_time();
            let max = state.get_max_time();

            if let (Some(min), Some(max)) = (min, max) {
                if max - min > 5. {
                    info!("Synchronizing to necessary ({}, {})!", min, max);

                    let payload = ServerMessage::new().with_time(min);

                    for (_, Connection { stream, .. }) in state.connections.iter() {
                        send_message(&**stream, payload).await?;
                    }
                }
            }
        }
    }
    Ok(())
}

async fn data_loop(mut commands: Receiver<Command>) -> Result<()> {
    let mut state = GlobalState::default();

    while let Some(command) = commands.next().await {
        if let Err(err) = process_command(command, &mut state).await {
            info!("error occured when processing command: {}", err);
        }
    }

    Ok(())
}

async fn connection_reader_loop(
    stream: &TcpStream,
    id: Id,
    commands: &mut Sender<Command>,
) -> Result<()> {
    let mut buf_stream = BufReader::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        let bytes_read = buf_stream
            .read_line(&mut line)
            .await
            .context("Connection lost")?;

        if bytes_read == 0 {
            Err(anyhow!("Read 0 bytes.")).context("Connection lost")?;
        }

        let msg = serde_json::from_str(&line).context("Client broke protocol")?;
        commands.send(Command::Message(id, msg)).await?;
    }
}

async fn connection_handler(
    stream: TcpStream,
    id: Id,
    mut commands: Sender<Command>,
) -> Result<()> {
    let addr = stream.peer_addr()?;
    info!("Connected with {}", addr);

    let stream = Arc::new(stream);

    commands
        .send(Command::NewConnection(id, Arc::clone(&stream)))
        .await?;

    let err = connection_reader_loop(&*stream, id, &mut commands).await;

    info!("Disconnecting because: {}", err.unwrap_err());
    commands.send(Command::Disconnection(id)).await?;

    Ok(())
}

#[derive(StructOpt)]
struct Config {
    address: String,
}

async fn run() -> Result<()> {
    let subscriber = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .finish();
    tracing::subscriber::set_global_default(subscriber)
        .expect("Installing subscriber failed");

    let config: Config = StructOpt::from_args();

    let tcp = TcpListener::bind(&config.address).await?;
    let (command_sender, command_receiver) = mpsc::unbounded();

    let _data_handle = task::spawn(data_loop(command_receiver));

    let mut sync_command_sender = command_sender.clone();
    let _sync_handle = task::spawn(async move {
        loop {
            task::sleep(Duration::from_secs(3)).await;
            sync_command_sender
                .send(Command::Synchronize)
                .await
                .unwrap();
        }
    });

    let mut next_id = 1;
    let mut incoming = tcp.incoming();
    while let Some(connection) = incoming.next().await {
        let connection = connection?;

        let command_sender = &command_sender;
        let addr = connection.peer_addr().unwrap().to_string();
        let addr = &addr[..];
        let id = next_id;
        let con_fut = connection_handler(connection, id, command_sender.clone())
            .instrument(info_span!("con", id, addr));

        task::spawn(con_fut);

        next_id += 1;
    }

    Ok(())
}

fn main() -> Result<()> {
    task::block_on(run())
}
