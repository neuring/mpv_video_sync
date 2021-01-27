use std::{fmt::Display, ops::Sub};

use derive_more::From;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, From)]
pub enum ClientMessage {
    Init(ClientInit),
    Update(ClientUpdate),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInit {
    pub video_hash: String,
    pub name: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ClientUpdate {
    Timestamp { time: Time },
    Seek { time: Time },
    Pause { time: Time },
    Resume { time: Time },
    SpeedChange { factor: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ServerDisconnect {
    IncorrectHash,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UpdateCause {
    UserAction(String), // Name of client responsible.
    Synchronize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerState {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<Time>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,

    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlayerUpdate {
    pub state: PlayerState,

    pub cause: UpdateCause,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum UserUpdate {
    Connected(String),
    Disconnected(String),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInit {
    pub player_state: PlayerState,

    pub users: Vec<String>,
}


#[derive(Debug, Clone, From, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Init(ServerInit),
    UserUpdate(UserUpdate),
    PlayerUpdate(PlayerUpdate),
    Disconnect(ServerDisconnect),
}

impl PlayerUpdate {
    pub fn new(cause: UpdateCause) -> Self {
        Self {
            state: PlayerState {
                time: None,
                speed: None,
                paused: None,
            },
            cause,
        }
    }

    pub fn with_time(mut self, t: Time) -> Self {
        self.state.time = Some(t);
        self
    }

    pub fn with_pause(mut self, p: bool) -> Self {
        self.state.paused = Some(p);
        self
    }

    pub fn with_speed(mut self, s: f64) -> Self {
        self.state.speed = Some(s);
        self
    }
}

/// Struct for storing the time of a played a video.
/// Used for better formatting.
/// It cannot store NaN.
#[derive(Debug, PartialEq, PartialOrd, Clone, Copy, Serialize, Deserialize)]
pub struct Time {
    seconds: f64,
}

impl Time {
    pub fn from_seconds(seconds: f64) -> Self {
        if seconds.is_nan() || seconds.is_infinite() {
            panic!("Time cannot be NaN or Infinite.");
        }
        Self { seconds }
    }

    pub fn zero() -> Self {
        Self { seconds: 0.0 }
    }

    pub fn as_seconds(&self) -> f64 {
        self.seconds
    }
}

impl Eq for Time {}

impl Ord for Time {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.seconds
            .partial_cmp(&other.seconds)
            .expect("Cannot fail, because time can never store NaN.")
    }
}

impl Display for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let total_seconds = self.seconds.floor() as u32;

        let seconds = total_seconds % 60;
        let minutes = (total_seconds / 60) % 60;
        let hours = (total_seconds / (60 * 60)) % 60;

        write!(f, "{:02}:{:02}:{:02}", hours, minutes, seconds)
    }
}

impl Sub for Time {
    type Output = f64;

    fn sub(self, rhs: Self) -> Self::Output {
        self.seconds - rhs.seconds
    }
}
