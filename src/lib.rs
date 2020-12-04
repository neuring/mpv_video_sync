use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ClientMessage {
    Timestamp { time: f64 },
    Seek { time: f64 },
    Pause { time: f64 },
    Resume { time: f64 },
    SpeedChange { factor: f64 },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ServerMessage {
    Update {
        #[serde(skip_serializing_if = "Option::is_none")]
        time: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        speed: Option<f64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        paused: Option<bool>,
    },
}


impl ServerMessage {
    pub fn new() -> Self {
        Self::Update {
            time: None,
            speed: None,
            paused: None,
        }
    }

    pub fn with_time(mut self, t: f64) -> Self {
        match &mut self {
            Self::Update { time, .. } => *time = Some(t),
        }
        self
    }

    pub fn with_pause(mut self, p: bool) -> Self {
        match &mut self {
            Self::Update { paused, .. } => *paused = Some(p),
        }
        self
    }

    pub fn with_speed(mut self, s: f64) -> Self {
        match &mut self {
            Self::Update { speed, .. } => *speed = Some(s),
        }
        self
    }
}
