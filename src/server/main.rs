use std::{
    collections::{hash_map::Entry, HashMap},
    net::{Shutdown, SocketAddr},
    sync::Arc,
    time::{Duration, Instant},
};

use anyhow::{anyhow, bail, Context, Result};
use async_std::{
    io::{
        prelude::{BufReadExt, WriteExt},
        BufReader,
    },
    net::{TcpListener, TcpStream},
    task,
};
use futures::{
    channel::mpsc::{
        self, UnboundedReceiver as Receiver, UnboundedSender as Sender,
    },
    SinkExt, StreamExt,
};
use structopt::StructOpt;
use tracing::{debug, info, info_span, warn, Instrument};
use tracing_subscriber::EnvFilter;
use video_sync::*;

#[derive(Debug, Clone, Copy)]
struct Timestamp {
    value: Time,
    when: Instant,
}

impl Timestamp {
    fn now(value: Time) -> Self {
        Self {
            value,
            when: Instant::now(),
        }
    }
}

#[derive(Debug)]
struct Connection {
    stream: Arc<TcpStream>,
    timestamp: Option<Timestamp>,
    peer: SocketAddr,
    state: ConnectionState,
}

#[derive(Debug, PartialEq, Eq)]
enum ConnectionState {
    Uninitialized,
    Initialized { name: String },
}

impl ConnectionState {
    fn init(&mut self, name: String) {
        assert_eq!(self, &Self::Uninitialized);

        *self = Self::Initialized { name };
    }

    fn get_name(&self) -> Option<&str> {
        match self {
            Self::Uninitialized => None,
            Self::Initialized { name } => Some(&name),
        }
    }
}

impl Default for ConnectionState {
    fn default() -> Self {
        Self::Uninitialized
    }
}

type Id = usize;

enum Command {
    NewConnection(Id, Arc<TcpStream>),
    Disconnection(Id),
    Message(Id, ClientMessage),
    Synchronize,
}

/// Shared state over all connections.
#[derive(Debug, Default)]
struct GlobalState {
    connections: HashMap<Id, Connection>,
    player: PlayerState,
}

impl GlobalState {
    // Get the minimum time of all connections and the id of this connection.
    // Returns None if there are no connections.
    fn get_min_time(&self) -> Option<(Id, Time)> {
        self.connections
            .iter()
            .filter_map(|(&id, con)| Some((id, con.timestamp?.value)))
            .min_by_key(|&(_, a)| a)
    }

    // Get the maximum time of all connections and the id of this connection.
    // Returns None if there are no connections.
    fn get_max_time(&self) -> Option<(Id, Time)> {
        self.connections
            .iter()
            .filter_map(|(&id, con)| Some((id, con.timestamp?.value)))
            .max_by_key(|&(_, a)| a)
    }

    // Collects a list of all initialised usernames.
    fn get_all_usernames(&self) -> Vec<String> {
        self.connections.values().filter_map(|c| match &c.state {
            ConnectionState::Uninitialized => None,
            ConnectionState::Initialized { name } => Some(name),
        })
        .cloned()
        .collect()
    }
}

/// Expected state of all client mpv players.
/// The time is ommited because it is kept for each client separately in `Connection`
#[derive(Debug)]
struct PlayerState {
    speed: f64,
    paused: bool,
    video_hash: Option<String>,
}

impl PlayerState {
    fn reset(&mut self) {
        self.video_hash = None;
    }
}

impl Default for PlayerState {
    fn default() -> Self {
        Self {
            speed: 1.0,
            paused: false,
            video_hash: None,
        }
    }
}

async fn send_message(
    mut stream: &TcpStream,
    msg: impl Into<ServerMessage>,
) -> Result<()> {
    let msg = msg.into();
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
            let min_time = state.get_min_time().map(|(_, t)| t).unwrap_or(Time::zero());
            let users = state.get_all_usernames();

            if let Entry::Vacant(e) = state.connections.entry(id) {
                let peer =
                    stream.peer_addr().context("Failed to extract peer addr.")?;

                let connection = e.insert(Connection {
                    stream,
                    peer,
                    timestamp: None,
                    state: ConnectionState::default(),
                });

                let player_state = video_sync::PlayerState {
                    time: Some(min_time),
                    speed: Some(state.player.speed),
                    paused: Some(state.player.paused),
                };

                let msg = ServerInit {
                    player_state,
                    users,
                };

                send_message(connection.stream.as_ref(), msg).await?;
            } else {
                bail!("Existing connection with id: {}", id);
            }
        }
        Command::Disconnection(id) => {
            let removed = state.connections.remove(&id).with_context(|| {
                format!("ID: {} doesn't exist in connections.", id)
            })?;

            if state.connections.is_empty() {

                state.player.reset()

            } else if let ConnectionState::Initialized{ name } = removed.state {

                let msg = ServerMessage::UserUpdate(UserUpdate::Disconnected(name));

                for con in state.connections.values() {
                    send_message(con.stream.as_ref(), msg.clone()).await?;
                }
            }
        }
        Command::Message(id, ClientMessage::Update(msg)) => match msg {
            ClientUpdate::Timestamp { time } => {
                state.connections.get_mut(&id).unwrap().timestamp =
                    Some(Timestamp::now(time));
            }
            ClientUpdate::Seek { .. }
            | ClientUpdate::Pause { .. }
            | ClientUpdate::Resume { .. }
            | ClientUpdate::SpeedChange { .. } => {
                let con = &state.connections[&id];
                debug!("Received {:?} from {} ({})", msg, con.peer, id);

                let name = con.state.get_name().ok_or(anyhow!(
                    "Received client update \
                        from uninitialized client. Ignoring..."
                ))?;
                let payload =
                    PlayerUpdate::new(UpdateCause::UserAction(name.to_string()));

                let payload = match msg {
                    ClientUpdate::Seek { time } => payload.with_time(time),
                    ClientUpdate::Pause { time } => {
                        payload.with_time(time).with_pause(true)
                    }
                    ClientUpdate::Resume { time } => {
                        payload.with_time(time).with_pause(false)
                    }
                    ClientUpdate::SpeedChange { factor } => {
                        payload.with_speed(factor)
                    }
                    _ => {
                        warn!("Unreachable match arm reached.");
                        return Ok(());
                    }
                };

                let mut payload = serde_json::to_string(&payload).unwrap();
                payload.push('\n');

                for (id, Connection { stream, peer, .. }) in state
                    .connections
                    .iter()
                    .filter(|&(&con_id, _)| con_id != id)
                {
                    debug!("Sending {:?} to {} ({})", msg, peer, id);

                    (&**stream).write_all(payload.as_bytes()).await?;
                }

                match msg {
                    ClientUpdate::Pause { .. } => state.player.paused = true,
                    ClientUpdate::Resume { .. } => state.player.paused = false,
                    ClientUpdate::SpeedChange { factor } => {
                        state.player.speed = factor
                    }
                    _ => {}
                }
            }
        },
        Command::Message(id, ClientMessage::Init(msg)) => {
            let con = state.connections.get_mut(&id).unwrap();

            debug!("Initializing {} ({}) with {:?}", con.peer, id, &msg.name);

            con.state.init(msg.name.clone());

            match &state.player.video_hash {
                Some(common_hash) if common_hash != &msg.video_hash => {
                    info!(
                        "{} ({}) has video hash different \
                          from other established connections.",
                        con.peer, id
                    );

                    send_message(&con.stream, ServerDisconnect::IncorrectHash)
                        .await?;

                    con.stream.shutdown(Shutdown::Both).with_context(|| {
                        format!("Forcible shutdown of {} ({}) failed.", con.peer, id)
                    })?;
                    return Ok(());
                }
                None => {
                    assert_eq!(
                        state
                            .connections
                            .values()
                            .filter(|c| &c.state != &ConnectionState::Uninitialized)
                            .count(),
                        1
                    );

                    state.player.video_hash = Some(msg.video_hash);
                }
                Some(_) => {}
            }

            let msg = ServerMessage::UserUpdate(UserUpdate::Connected(msg.name));
            for (&con_id, con) in state.connections.iter() {
                if id == con_id {
                    continue
                }
                send_message(con.stream.as_ref(), msg.clone()).await?;
            }
        }

        Command::Synchronize => {
            let min = state.get_min_time();
            let max = state.get_max_time();

            if let (Some((min_id, min)), Some((max_id, max))) = (min, max) {
                if max - min > 5. {
                    let max_con = &state.connections[&max_id];
                    let min_con = &state.connections[&min_id];

                    info!(
                        "Synchronizing necessary {} ({}) at {} and {} ({}) at max {}!",
                        min_con.peer, min_id, min, max_con.peer, max_id, max
                    );

                    let payload =
                        PlayerUpdate::new(UpdateCause::Synchronize).with_time(min);

                    for (_, Connection { stream, .. }) in state.connections.iter() {
                        send_message(&**stream, payload.clone()).await?;
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

        let msg = serde_json::from_str(&line).with_context(|| {
            format!("Client broke protocol: \"{}\"", line.trim())
        })?;
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
