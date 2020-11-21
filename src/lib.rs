use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum ClientMessage {
    Timestamp { time: f64 },
    Seek { time: f64 },
    Pause { time: f64 },
    Resume { time: f64 },
    SpeedChange { factor: f64 },
}
