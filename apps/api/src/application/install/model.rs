use super::super::{ApplicationCommand, CommandResult, InstallVersionPayload};
use crate::observability::OperationProofRecord;
use crate::state::contracts::OperationId;
use croopor_minecraft::{DownloadProgress, LoaderComponentId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionStaging {
    pub command: ApplicationCommand,
    pub result: CommandResult<InstallVersionPayload>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallGuardianRepairSummary {
    pub repair_operation_id: OperationId,
    pub diagnosis_id: String,
    pub status: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallGuardianOutcomeSummary {
    pub diagnosis_id: String,
    pub decision: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub guidance: Vec<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionStartRequest {
    pub version_id: String,
    #[serde(default)]
    pub manifest_url: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoaderInstallStartRequest {
    pub component_id: LoaderComponentId,
    pub build_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoaderBuildsRequest {
    pub component_id: LoaderComponentId,
    pub mc_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallStartResponse {
    pub install_id: String,
    pub operation_id: OperationId,
    pub view_model: InstallProgressViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallProgressStepViewModel {
    pub phase_id: String,
    pub label: String,
    pub progress_pct: u8,
    pub current: i32,
    pub total: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallProgressViewModel {
    pub phase_id: String,
    pub label: String,
    pub progress_pct: u8,
    pub terminal: bool,
    pub failed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_step: Option<InstallProgressStepViewModel>,
}

impl InstallProgressViewModel {
    pub fn starting() -> Self {
        Self {
            phase_id: "queued".to_string(),
            label: "Preparing install".to_string(),
            progress_pct: 0,
            terminal: false,
            failed: false,
            active_step: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallStatusResponse {
    pub install_id: String,
    pub operation_id: OperationId,
    pub done: bool,
    pub progress: Vec<DownloadProgress>,
    pub view_model: InstallProgressViewModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_view_model: Option<InstallFailureViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_point: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian: Option<InstallGuardianOutcomeSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guardian_repair: Option<InstallGuardianRepairSummary>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub proof: Option<OperationProofRecord>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallActionViewModel {
    pub action: String,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallFailureViewModel {
    pub state_id: String,
    pub title: String,
    pub tone: String,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub details: Vec<String>,
    pub retry_action: InstallActionViewModel,
    pub dismiss_action: InstallActionViewModel,
    pub repair_action: InstallActionViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueRequest {
    pub kind: String,
    #[serde(default)]
    pub version_id: String,
    #[serde(default)]
    pub manifest_url: String,
    #[serde(default)]
    pub component_id: String,
    #[serde(default)]
    pub build_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueInstallItemViewModel {
    pub version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<InstallQueueLoaderItemViewModel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueLoaderItemViewModel {
    pub component_id: String,
    pub build_id: String,
    pub minecraft_version: String,
    pub loader_version: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueuedItemViewModel {
    pub queue_id: String,
    pub state_id: String,
    pub kind: String,
    pub title: String,
    pub label: String,
    pub summary: String,
    pub detail: String,
    pub position: usize,
    pub total: usize,
    pub install_item: InstallQueueInstallItemViewModel,
    pub remove_action: InstallActionViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueActiveViewModel {
    pub queue_id: String,
    pub install_id: String,
    pub operation_id: OperationId,
    pub kind: String,
    pub title: String,
    pub label: String,
    pub summary: String,
    pub install_item: InstallQueueInstallItemViewModel,
    pub progress: InstallProgressViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueViewModel {
    pub state_id: String,
    pub status_label: String,
    pub title: String,
    pub summary: String,
    pub queued_count: usize,
    pub queued_count_label: String,
    pub queued_item_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_queued_count_label: Option<String>,
    pub section_title: String,
    pub empty_title: String,
    pub empty_summary: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueNoticeViewModel {
    pub state_id: String,
    pub tone: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueStateResponse {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<InstallQueueActiveViewModel>,
    pub items: Vec<InstallQueuedItemViewModel>,
    pub view_model: InstallQueueViewModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<InstallQueueNoticeViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_install: Option<InstallStartResponse>,
}
