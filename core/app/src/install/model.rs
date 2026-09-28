pub use crate::content::resolve::ResolutionSelection as InstallQueueContentSelection;
use crate::public::OperationId;
use axial_minecraft::{DownloadProgress, LoaderComponentId};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallVersionStartRequest {
    pub version_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct LoaderInstallStartRequest {
    pub component_id: LoaderComponentId,
    pub build_id: String,
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
            phase_id: "starting".to_string(),
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
    pub revision: u64,
    pub queue_id: String,
    pub outcome: Option<InstallOutcome>,
    pub allowed_actions: Vec<InstallActionViewModel>,
    pub install_id: String,
    pub operation_id: OperationId,
    pub done: bool,
    pub progress: Vec<DownloadProgress>,
    pub view_model: InstallProgressViewModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_view_model: Option<InstallFailureViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure_point: Option<String>,
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
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum InstallQueueRequest {
    Vanilla {
        version_id: String,
    },
    Loader {
        component_id: LoaderComponentId,
        build_id: String,
    },
    Content {
        instance_id: String,
        label: String,
        action: InstallQueueContentActionRequest,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[serde(deny_unknown_fields)]
pub enum InstallQueueContentActionRequest {
    Install {
        selections: Vec<InstallQueueContentSelection>,
        #[serde(default)]
        allow_incompatible: bool,
    },
    Uninstall {
        canonical_ids: Vec<String>,
    },
    Modpack {
        canonical_id: String,
        version_id: String,
        #[serde(default)]
        selected_file_ids: Vec<String>,
        #[serde(default)]
        include_overrides: bool,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueInstallItemViewModel {
    pub version_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub loader: Option<InstallQueueLoaderItemViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<InstallQueueContentItemViewModel>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueContentItemViewModel {
    pub instance_id: String,
    pub label: String,
    pub action: InstallQueueContentActionRequest,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_action: Option<InstallActionViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<OperationId>,
    /// Unix epoch milliseconds for when the install session began running.
    /// Omitted while the queue entry is only reserved in the active lane.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub install_started_at_ms: Option<u64>,
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
    /// A fresh queue incarnation permits clients to rebase after process restart.
    pub queue_epoch: String,
    pub revision: u64,
    /// Changes whenever a completed operation may change installed/instance views.
    pub registry_revision: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_failure: Option<InstallQueueFailureViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active: Option<InstallQueueActiveViewModel>,
    pub items: Vec<InstallQueuedItemViewModel>,
    pub view_model: InstallQueueViewModel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<InstallQueueNoticeViewModel>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_install: Option<InstallStartResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_instance_id: Option<String>,
}

/// A terminal result is emitted only after file and metadata effects settle.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallOutcome {
    Succeeded,
    Failed,
    Cancelled,
    Removed,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallQueueFailureViewModel {
    pub failed_at_ms: u64,
    pub queue_id: String,
    pub install_id: String,
    pub operation_id: OperationId,
    pub label: String,
    pub install_item: InstallQueueInstallItemViewModel,
    pub failure_view_model: InstallFailureViewModel,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct InstallEvent<T> {
    pub revision: u64,
    pub value: T,
}

impl InstallActionViewModel {
    pub fn enabled(action: &str, label: &str) -> Self {
        Self {
            action: action.into(),
            label: label.into(),
            enabled: true,
            disabled_reason: None,
        }
    }
}
