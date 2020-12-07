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
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ClientUpdate {
    Timestamp { time: f64 },
    Seek { time: f64 },
    Pause { time: f64 },
    Resume { time: f64 },
    SpeedChange { factor: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ServerDisconnect {
    IncorrectHash,
}

#[derive(Debug, Default, Clone, Copy, Serialize, Deserialize)]
pub struct ServerUpdate{
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub paused: Option<bool>,
}

#[derive(Debug, Clone, Copy, From, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Update(ServerUpdate),
    Disconnect(ServerDisconnect),
}

impl ServerUpdate {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_time(mut self, t: f64) -> Self {
        match &mut self {
            Self { time, .. } => *time = Some(t),
        }
        self
    }

    pub fn with_pause(mut self, p: bool) -> Self {
        match &mut self {
            Self { paused, .. } => *paused = Some(p),
        }
        self
    }

    pub fn with_speed(mut self, s: f64) -> Self {
        match &mut self {
            Self { speed, .. } => *speed = Some(s),
        }
        self
    }
}
