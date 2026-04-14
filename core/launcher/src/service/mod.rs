mod healing;
mod mapping;
mod prepare;
mod validation;

use crate::build::VanillaLaunchPlan;
use crate::healing::HealingEvent;
use crate::runtime::RuntimeSelection;
use crate::types::LaunchFailureClass;
use serde::{Deserialize, Serialize};

pub use healing::{
    HealingSummaryInput, build_healing_summary, conservative_healing_preset, infer_loader,
    recovery_for_failure,
};
pub use mapping::{
    failure_class_name, format_failure_class, is_terminal_state, is_terminal_status,
    launch_state_name, snapshot_status,
};
pub use prepare::{prepare_launch_attempt, sanitize_effective_runtime_major};

#[derive(Debug, Clone)]
pub struct LaunchIntent {
    pub session_id: String,
    pub library_dir: std::path::PathBuf,
    pub instance_id: String,
    pub version_id: String,
    pub username: String,
    pub requested_java: String,
    pub requested_preset: String,
    pub extra_jvm_args: Vec<String>,
    pub max_memory_mb: i32,
    pub min_memory_mb: i32,
    pub resolution: Option<(u32, u32)>,
    pub launcher_name: String,
    pub launcher_version: String,
    pub game_dir: Option<std::path::PathBuf>,
    pub advanced_overrides: bool,
    pub performance_mode: String,
}

#[derive(Debug, Clone, Default)]
pub struct AttemptOverrides {
    pub force_managed_runtime: bool,
    pub disable_custom_gc: bool,
    pub preset_override: Option<String>,
    pub fallback_applied: Option<String>,
    pub retry_count: u32,
}

#[derive(Debug, Clone)]
pub struct PreparedLaunchAttempt {
    pub runtime: RuntimeSelection,
    pub effective_preset: String,
    pub plan: VanillaLaunchPlan,
    pub healing: Option<LaunchHealingSummary>,
    pub metrics: LaunchPreparationMetrics,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LaunchPreparationMetrics {
    pub version_ms: u128,
    pub runtime_ms: u128,
    pub planning_ms: u128,
    pub total_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct LaunchHealingSummary {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_preset: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requested_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub effective_java_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auth_mode: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fallback_applied: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure_class: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub advanced_overrides: Option<bool>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub events: Vec<HealingEvent>,
}

#[derive(Debug, Clone)]
pub struct LaunchPreparationError {
    pub message: String,
    pub failure_class: Option<LaunchFailureClass>,
    pub healing: Option<LaunchHealingSummary>,
}

#[derive(Debug, Clone)]
pub struct RecoveryPlan {
    pub description: String,
    pub action: RecoveryAction,
}

#[derive(Debug, Clone)]
pub enum RecoveryAction {
    DowngradePreset(String),
    DisableCustomGc,
    SwitchManagedRuntime,
}
