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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<LaunchStageEvidence>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchStageEvidence {
    pub id: String,
    pub system: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchSessionOutcomeKind {
    Clean,
    Stopped,
    Failed,
    Unknown,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchSessionExitReason {
    CleanExit,
    ExternalUserClosed,
    LauncherStopped,
    SpawnFailed,
    StartupFailed,
    StartupStalled,
    WatchdogKilled,
    CrashedBeforeBoot,
    CrashedAfterBoot,
    UnknownExit,
}

impl LaunchSessionExitReason {
    pub fn kind(self) -> LaunchSessionOutcomeKind {
        match self {
            Self::CleanExit | Self::ExternalUserClosed => LaunchSessionOutcomeKind::Clean,
            Self::LauncherStopped => LaunchSessionOutcomeKind::Stopped,
            Self::SpawnFailed
            | Self::StartupFailed
            | Self::StartupStalled
            | Self::WatchdogKilled
            | Self::CrashedBeforeBoot
            | Self::CrashedAfterBoot => LaunchSessionOutcomeKind::Failed,
            Self::UnknownExit => LaunchSessionOutcomeKind::Unknown,
        }
    }

    pub fn summary(self) -> &'static str {
        match self {
            Self::CleanExit => "Minecraft exited cleanly.",
            Self::ExternalUserClosed => "Minecraft was closed outside the launcher after startup.",
            Self::LauncherStopped => "The launcher stopped the session.",
            Self::SpawnFailed => "The game process could not be started.",
            Self::StartupFailed => "Minecraft failed during startup.",
            Self::StartupStalled => "Minecraft did not finish startup in time.",
            Self::WatchdogKilled => "Guardian stopped a stalled startup.",
            Self::CrashedBeforeBoot => "Minecraft exited before startup completed.",
            Self::CrashedAfterBoot => "Minecraft crashed after startup.",
            Self::UnknownExit => "Minecraft exited and the launcher could not classify the reason.",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchSessionOutcome {
    pub reason: LaunchSessionExitReason,
    pub kind: LaunchSessionOutcomeKind,
    pub summary: String,
}

impl LaunchSessionOutcome {
    pub fn from_reason(reason: LaunchSessionExitReason) -> Self {
        Self {
            reason,
            kind: reason.kind(),
            summary: reason.summary().to_string(),
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LaunchNoticeTone {
    Info,
    Success,
    Warned,
    Intervened,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LaunchNotice {
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    pub tone: LaunchNoticeTone,
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
    #[serde(skip_serializing_if = "Option::is_none")]
    pub outcome: Option<LaunchSessionOutcome>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notice: Option<LaunchNotice>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence: Vec<LaunchStageEvidence>,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LaunchPriorityEvidence {
    pub start_mode: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub start_error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub promotion_error: Option<String>,
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
    pub priority: Option<LaunchPriorityEvidence>,
    pub exit_code: Option<i32>,
    pub command: Vec<String>,
    pub java_path: Option<String>,
    pub natives_dir: Option<String>,
    pub failure: Option<LaunchFailure>,
    pub healing: Option<serde_json::Value>,
    pub guardian: Option<serde_json::Value>,
    pub outcome: Option<LaunchSessionOutcome>,
    pub stages: Vec<LaunchStageRecord>,
}
