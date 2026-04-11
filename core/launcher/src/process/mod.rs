use crate::types::{LaunchFailure, LaunchState, SessionId};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct LaunchStatusEvent {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_detail: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub healing: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchLogEvent {
    pub source: String,
    pub text: String,
}

#[derive(Debug, Clone)]
pub enum LaunchEvent {
    Status(LaunchStatusEvent),
    Log(LaunchLogEvent),
}

impl LaunchEvent {
    pub fn event_type(&self) -> &'static str {
        match self {
            Self::Status(_) => "status",
            Self::Log(_) => "log",
        }
    }
}

#[derive(Debug, Clone)]
pub struct LaunchSessionRecord {
    pub session_id: SessionId,
    pub instance_id: String,
    pub version_id: String,
    pub state: LaunchState,
    pub pid: Option<u32>,
    pub exit_code: Option<i32>,
    pub command: Vec<String>,
    pub java_path: Option<String>,
    pub natives_dir: Option<String>,
    pub failure: Option<LaunchFailure>,
    pub healing: Option<serde_json::Value>,
}
