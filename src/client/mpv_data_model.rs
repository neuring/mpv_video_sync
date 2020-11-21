use serde::{Deserialize, Serialize};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum MpvCommand {
    GetProperty {
        request_id: u64,
        property: MpvProperty,
    },
    SetProperty {
        request_id: u64,
        property: MpvPropertyValue,
    },
    ObserveProperty {
        request_id: u64,
        id: u64, // not sure what this does
        property: MpvProperty,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(tag = "name", content = "data")]
pub enum MpvPropertyValue {
    #[serde(rename = "pause")]
    Pause(bool),
    #[serde(rename = "time-pos")]
    TimePos(f64), //TODO: Maybe use playback time
    #[serde(rename = "speed")]
    Speed(f64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum MpvProperty {
    #[serde(rename = "pause")]
    Pause,
    #[serde(rename = "time-pos")]
    TimePos, //TODO: Maybe use playback time
    #[serde(rename = "speed")]
    Speed,
}

impl MpvProperty {
    fn to_string(&self) -> &'static str {
        match self {
            MpvProperty::Pause => "pause",
            MpvProperty::TimePos => "time-pos",
            MpvProperty::Speed => "speed",
        }
    }
}

impl MpvCommand {
    pub fn to_json_command(&self) -> serde_json::Value {
        match self {
            MpvCommand::GetProperty {
                request_id,
                property,
            } => json! ({
                "command": ["get_property", property.to_string()],
                "request_id": request_id,
            }),
            MpvCommand::SetProperty {
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
            MpvCommand::ObserveProperty {
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

impl From<MpvPropertyValue> for MpvProperty {
    fn from(val: MpvPropertyValue) -> Self {
        match val {
            MpvPropertyValue::Pause(_) => MpvProperty::Pause,
            MpvPropertyValue::TimePos(_) => MpvProperty::TimePos,
            MpvPropertyValue::Speed(_) => MpvProperty::Speed,
        }
    }
}

impl MpvPropertyValue {
    fn to_cmd_list(&self) -> Vec<serde_json::Value> {
        let p: MpvProperty = (*self).into();
        match *self {
            MpvPropertyValue::Pause(paused) => {
                vec![p.to_string().into(), paused.into()]
            }
            MpvPropertyValue::TimePos(time) => {
                vec![p.to_string().into(), time.into()]
            }
            MpvPropertyValue::Speed(speed) => {
                vec![p.to_string().into(), speed.into()]
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct MpvResponse {
    pub request_id: u64,
    pub error: MpvErrorStatus,
    pub data: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub enum MpvErrorStatus {
    #[serde(rename = "success")]
    Success,
    #[serde(rename = "invalid parameter")]
    InvalidParameter,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "event")]
pub enum MpvEvent {
    #[serde(rename = "property-change")]
    PropertyChange {
        id: u64,
        #[serde(flatten)]
        name: MpvPropertyValue,
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
pub enum MpvResponseOrEvent {
    Event(MpvEvent),
    Response(MpvResponse),
}
