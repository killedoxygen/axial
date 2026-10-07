//! Public health is derived from exact admitted leaf inspection.

use super::model::*;
use axial_performance::{ManagedCompositionInspection, ManagedResolvedInspection};
use serde::Serialize;

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
