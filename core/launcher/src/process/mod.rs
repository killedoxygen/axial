use crate::types::{LaunchFailure, LaunchState, SessionId};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchStageRecord {
    pub stage: String,
    pub label: String,
    pub started_at_ms: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LaunchStatusEvent {
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub benchmark: Option<serde_json::Value>,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardian: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stages: Vec<LaunchStageRecord>,
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
    pub launched_at: Option<String>,
    pub benchmark: Option<serde_json::Value>,
    pub state: LaunchState,
    pub pid: Option<u32>,
    pub process_started_at_ms: Option<u64>,
    pub boot_completed_at_ms: Option<u64>,
    pub boot_duration_ms: Option<u64>,
    pub exit_code: Option<i32>,
    pub command: Vec<String>,
    pub java_path: Option<String>,
    pub natives_dir: Option<String>,
    pub failure: Option<LaunchFailure>,
    pub healing: Option<serde_json::Value>,
    pub guardian: Option<serde_json::Value>,
    pub stages: Vec<LaunchStageRecord>,
}
