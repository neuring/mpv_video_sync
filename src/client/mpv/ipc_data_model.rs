use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MpvIpcCommand {
    GetProperty {
        request_id: u64,
        property: MpvIpcProperty,
    },
    SetProperty {
        request_id: u64,
        property: MpvIpcPropertyValue,
    },
    ObserveProperty {
        request_id: u64,
        id: u64, // not sure what this does
        property: MpvIpcProperty,
    },
}

impl MpvIpcCommand {
    pub fn get_request_id(&self) -> u64 {
        match self {
            MpvIpcCommand::GetProperty { request_id, .. } => *request_id,
            MpvIpcCommand::SetProperty { request_id, .. } => *request_id,
            MpvIpcCommand::ObserveProperty { request_id, .. } => *request_id,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum MpvIpcPropertyValue {
    #[serde(rename = "pause")]
    Pause(bool),
    #[serde(rename = "time-pos")]
    TimePos(f64), //TODO: Maybe use playback time
    #[serde(rename = "speed")]
    Speed(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MpvIpcProperty {
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "time-pos")]
    TimePos, //TODO: Maybe use playback time
    #[serde(rename = "speed")]
    Speed,
}

impl MpvIpcProperty {
    fn to_string(&self) -> &'static str {
        match self {
            MpvIpcProperty::Pause => "pause",
            MpvIpcProperty::TimePos => "time-pos",
            MpvIpcProperty::Speed => "speed",
        }
    }
}

impl MpvIpcCommand {
    pub fn to_json_command(&self) -> serde_json::Value {
        match self {
            MpvIpcCommand::GetProperty {
                request_id,
                property,
            } => json! ({
                "command": ["get_property", property.to_string()],
                "request_id": request_id,
            }),
            MpvIpcCommand::SetProperty {
                request_id,
                property,
            } => {
                let mut cmd = property.to_cmd_list();
                cmd.insert(0, "set_property".into());
                json!({
                    "command": cmd,
                    "request_id": request_id,
                })
            }
            MpvIpcCommand::ObserveProperty {
                request_id,
                id,
                property,
            } => json! ({
                "command": ["observe_property", id, property.to_string()],
                "request_id": request_id,
            }),
        }
    }
}

impl From<MpvIpcPropertyValue> for MpvIpcProperty {
    fn from(val: MpvIpcPropertyValue) -> Self {
        match val {
            MpvIpcPropertyValue::Pause(_) => MpvIpcProperty::Pause,
            MpvIpcPropertyValue::TimePos(_) => MpvIpcProperty::TimePos,
            MpvIpcPropertyValue::Speed(_) => MpvIpcProperty::Speed,
        }
    }
}

impl MpvIpcPropertyValue {
    fn to_cmd_list(&self) -> Vec<serde_json::Value> {
        let p: MpvIpcProperty = (*self).into();
        match *self {
            MpvIpcPropertyValue::Pause(paused) => {
                vec![p.to_string().into(), paused.into()]
            }
            MpvIpcPropertyValue::TimePos(time) => {
                vec![p.to_string().into(), time.into()]
            }
            MpvIpcPropertyValue::Speed(speed) => {
                vec![p.to_string().into(), speed.into()]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MpvIpcResponse {
    pub request_id: u64,
    pub error: MpvIpcErrorStatus,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum MpvIpcErrorStatus {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "invalid parameter")]
    InvalidParameter,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "event")]
pub enum MpvIpcEvent {
    #[serde(rename = "property-change")]
    PropertyChange {
        id: u64,
        #[serde(flatten)]
        name: MpvIpcPropertyValue,
    },
    #[serde(rename = "playback-restart")]
    PlaybackRestart,
    #[serde(rename = "seek")]
    Seek,
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "unpause")]
    Unpause,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(untagged)]
pub enum MpvIpcResponseOrEvent {
    Event(MpvIpcEvent),
    Response(MpvIpcResponse),
}
