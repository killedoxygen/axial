//! Public health is derived from exact admitted leaf inspection.

use super::composition::{
    ManagedCompositionInspection, ManagedResolvedInspection, storage::ManagedStorageDirectory,
};
use super::{model::*, mutation::PerformanceOperationStatus};
use serde::Serialize;
use std::collections::BTreeSet;

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceHealthResponse {
    pub state: Option<CompositionState>,
    pub health: BundleHealth,
    pub warnings: Vec<String>,
    pub managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    pub rollback_available: bool,
    pub view_model: PerformancePlanSummaryViewModel,
}

/// Selected non-managed mode does not inspect or settle retained managed files.
pub fn disabled_health_response() -> PerformanceHealthResponse {
    PerformanceHealthResponse {
        state: None,
        health: BundleHealth::Disabled,
        warnings: Vec::new(),
        managed_artifacts: Vec::new(),
        rollback_available: false,
        view_model: PerformancePlanSummaryViewModel {
            state_id: "performance_summary_disabled".into(),
            title: "No managed bundle".into(),
            detail: "Memory allocation and Java detection are shown below.".into(),
            tone: ViewModelTone::Mute,
            health: Some("disabled".into()),
            composition_id: None,
            managed_artifact_count: 0,
            actions: Vec::new(),
        },
    }
}

pub fn resolved_health_response(
    mut resolved: ManagedResolvedInspection,
) -> PerformanceHealthResponse {
    resolved
        .plan
        .warnings
        .append(&mut resolved.inspection.warnings);
    resolved.inspection.warnings = resolved.plan.warnings;
    health_response(resolved.inspection)
}

pub fn health_response(inspection: ManagedCompositionInspection) -> PerformanceHealthResponse {
    let count = inspection
        .state
        .as_ref()
        .map_or(0, |state| state.installed_mods.len());
    let (state_id, title, detail, tone) = match inspection.health {
        BundleHealth::Healthy => (
            "healthy",
            "Performance is ready",
            "Managed files match their recorded composition.",
            ViewModelTone::Ok,
        ),
        BundleHealth::Disabled => (
            "disabled",
            "Managed files are not installed",
            "Apply a managed plan to install its performance files.",
            ViewModelTone::Mute,
        ),
        BundleHealth::Invalid => (
            "invalid",
            "Performance needs attention",
            "The managed bundle could not be validated.",
            ViewModelTone::Warn,
        ),
    };
    let managed_artifacts = inspection
        .state
        .iter()
        .flat_map(|state| &state.installed_mods)
        .map(|artifact| PerformanceManagedArtifactSummary {
            project_id: artifact.project_id.clone(),
            version_id: artifact.version_id.clone(),
            filename: artifact.filename.clone(),
            ownership_class: artifact.ownership_class,
            source_provider: artifact.source.provider,
            role: artifact.role,
            size: artifact.size,
        })
        .collect();
    PerformanceHealthResponse {
        managed_artifacts,
        rollback_available: inspection
            .rollback_snapshots
            .iter()
            .any(|snapshot| snapshot.rollback_available),
        view_model: PerformancePlanSummaryViewModel {
            state_id: state_id.into(),
            title: title.into(),
            detail: detail.into(),
            tone,
            health: Some(state_id.into()),
            composition_id: inspection
                .state
                .as_ref()
                .map(|state| state.composition_id.clone()),
            managed_artifact_count: count,
            actions: vec![ViewModelAction {
                command: PerformanceCommand::ApplyPerformancePlan,
                action: Some(
                    if inspection.state.is_some() {
                        "reapply"
                    } else {
                        "apply"
                    }
                    .into(),
                ),
                label: if inspection.state.is_some() {
                    "Reapply"
                } else {
                    "Apply"
                }
                .into(),
                enabled: true,
                disabled_reason: None,
            }],
        },
        state: inspection.state,
        health: inspection.health,
        warnings: inspection.warnings,
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceOperationView {
    #[serde(flatten)]
    pub operation: PerformanceOperationStatus,
    pub view_model: PerformanceOperationViewModel,
}

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceOperationViewModel {
    pub state_label: String,
    pub tone: &'static str,
    pub title: &'static str,
    pub detail: String,
    pub progress: PerformanceOperationProgress,
    pub is_terminal: bool,
    pub is_complete: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceOperationProgress {
    pub phase: String,
    pub current: u8,
    pub total: u8,
    pub done: bool,
}

impl From<PerformanceOperationStatus> for PerformanceOperationView {
    fn from(operation: PerformanceOperationStatus) -> Self {
        let complete = operation.state == "complete";
        let terminal = matches!(
            operation.state.as_str(),
            "complete" | "failed" | "interrupted"
        );
        let view_model = PerformanceOperationViewModel {
            state_label: operation.state.clone(),
            tone: if complete {
                "ok"
            } else if terminal {
                "warn"
            } else {
                "mute"
            },
            title: "Performance operation",
            detail: operation.error.clone().unwrap_or_default(),
            progress: PerformanceOperationProgress {
                phase: operation.state.clone(),
                current: terminal.into(),
                total: 1,
                done: terminal,
            },
            is_terminal: terminal,
            is_complete: complete,
        };
        Self {
            operation,
            view_model,
        }
    }
}

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceInstanceOperationResponse {
    pub operation: Option<PerformanceOperationView>,
}

pub(crate) fn derive_health(
    state: Option<&CompositionState>,
    plan: Option<&CompositionPlan>,
    expected_game_version: Option<&str>,
    instance_mods: Option<&ManagedStorageDirectory>,
) -> (BundleHealth, Vec<String>) {
    let Some(state) = state else {
        return (BundleHealth::Disabled, Vec::new());
    };

    if let Err(error) = super::composition::plan::validate_state_graph(state) {
        return (
            BundleHealth::Invalid,
            vec![format!("managed composition graph is invalid: {error}")],
        );
    }

    if let Some(plan) = plan
        && (!declarative_plan_matches_state(plan, state)
            || expected_game_version.is_some_and(|expected| expected != state.game_version))
    {
        return (
            BundleHealth::Invalid,
            vec!["managed composition does not match the current declarative plan".to_string()],
        );
    }

    let Some(instance_mods) = instance_mods else {
        return (
            BundleHealth::Invalid,
            vec!["managed composition storage authority is unavailable".to_string()],
        );
    };
    let mut warnings = Vec::new();
    for installed in &state.installed_mods {
        match super::composition::state::managed_artifact_matches(instance_mods, installed) {
            Ok(true) => {}
            Ok(false) => warnings.push(format!(
                "{} is missing or failed exact integrity validation",
                installed.filename
            )),
            Err(_) => warnings.push(format!("{} could not be validated", installed.filename)),
        }
    }
    if warnings.is_empty() {
        (BundleHealth::Healthy, warnings)
    } else {
        (BundleHealth::Invalid, warnings)
    }
}

fn declarative_plan_matches_state(plan: &CompositionPlan, state: &CompositionState) -> bool {
    let expected_roots = plan
        .mods
        .iter()
        .map(|managed_mod| managed_mod.project_id.as_str())
        .collect::<BTreeSet<_>>();
    let installed_roots = state
        .installed_mods
        .iter()
        .filter(|installed| installed.role == ManagedArtifactRole::Root)
        .map(|installed| installed.project_id.as_str())
        .collect::<BTreeSet<_>>();
    plan.composition_id == state.composition_id
        && plan.family == state.family
        && plan.tier == state.tier
        && plan.loader == state.loader
        && expected_roots == installed_roots
}
