//! Performance wire data and canonical leaf models. Filesystem authorities stay private.

pub use axial_performance::types::*;
pub use axial_performance::{
    BundleHealth, EffectivePerformancePlan, PerformanceRulesStatus, RuleChannel, RuleSource,
    RulesCacheState, RulesCacheStatus, RulesValidation,
};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerformancePlanRequest {
    pub game_version: Option<String>,
    pub loader: Option<String>,
    pub mode: Option<String>,
    pub instance_id: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformancePlanResponse {
    pub active: bool,
    pub effective: EffectivePerformancePlan,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ViewModelTone {
    Ok,
    Warn,
    Err,
    Mute,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ViewModelAction {
    pub command: PerformanceCommand,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub action: Option<String>,
    pub label: String,
    pub enabled: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum PerformanceCommand {
    ApplyPerformancePlan,
    RefreshPerformanceRules,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum RollbackState {
    NotApplicable,
    Unavailable,
    Available,
    Applied,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PerformancePlanSummaryViewModel {
    pub state_id: String,
    pub title: String,
    pub detail: String,
    pub tone: ViewModelTone,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub health: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub composition_id: Option<String>,
    #[serde(default)]
    pub managed_artifact_count: usize,
    #[serde(default)]
    pub actions: Vec<ViewModelAction>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceRulesStatusResponse {
    #[serde(flatten)]
    pub status: PerformanceRulesStatus,
    pub view_model: PerformanceRulesStatusViewModel,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceRulesStatusViewModel {
    pub source_label: String,
    pub channel_label: String,
    pub validation_label: String,
    pub validation_tone: ViewModelTone,
    pub validation_icon: String,
    pub summary: String,
    pub refresh_label: String,
    pub generated_label: String,
    pub cache_label: String,
    pub emergency_disable_label: String,
    pub details_label: String,
    pub health_states_label: String,
    pub ownership_label: String,
    pub warnings: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PerformanceManagedArtifactSummary {
    pub project_id: String,
    pub version_id: String,
    pub filename: String,
    pub ownership_class: OwnershipClass,
    pub source_provider: ManagedArtifactProvider,
    pub role: ManagedArtifactRole,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceInstanceDisplay {
    pub memory: PerformanceMemoryDisplay,
    pub runtime: PerformanceRuntimeDisplay,
    pub mode: PerformanceModeDisplay,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceMemoryDisplay {
    pub min_gb: f32,
    pub max_gb: f32,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceRuntimeDisplay {
    pub detected: bool,
    pub label: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PerformanceModeDisplay {
    pub mode: String,
    pub label: String,
    pub source: String,
    pub source_label: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ModeSource {
    Request,
    Instance,
    Global,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ResolvedPerformanceMode {
    pub mode: PerformanceMode,
    pub source: ModeSource,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PlanInputError {
    #[error("invalid performance mode")]
    InvalidMode,
    #[error(
        "instance version metadata is unavailable; install the version before resolving performance files"
    )]
    VersionUnavailable,
    #[error("game_version query parameter is required")]
    GameVersionRequired,
}
