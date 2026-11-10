//! Public health is derived from exact admitted leaf inspection.

use super::{model::*, mutation::PerformanceOperationStatus};
use axial_performance::{
    ManagedCompositionInspection, ManagedResolvedInspection, RollbackSnapshotSummary,
};
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

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceRollbackResponse {
    pub snapshots: Vec<RollbackSnapshotSummary>,
}

#[derive(Clone, Debug, Serialize)]
pub struct PerformanceInstallResponse {
    pub active: bool,
    pub status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    pub health: BundleHealth,
    pub composition_id: String,
    pub tier: &'static str,
    pub installed_count: usize,
    pub managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    pub warnings: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation: Option<PerformanceOperationView>,
}

impl PerformanceInstallResponse {
    /// Accepted durable work; its operation reports progress.
    pub fn queued(operation: PerformanceOperationStatus) -> Self {
        Self {
            active: true,
            status: "queued",
            install_id: Some(operation.id.clone()),
            health: BundleHealth::Disabled,
            composition_id: String::new(),
            tier: "",
            installed_count: 0,
            managed_artifacts: Vec::new(),
            warnings: Vec::new(),
            operation: Some(operation.into()),
        }
    }

    pub fn complete(inspection: ManagedCompositionInspection) -> Self {
        let state = inspection.state.as_ref();
        Self {
            active: state.is_some(),
            status: "complete",
            install_id: None,
            health: inspection.health,
            composition_id: state
                .map(|state| state.composition_id.clone())
                .unwrap_or_default(),
            tier: state.map_or("", |state| match state.tier {
                CompositionTier::Core => "core",
                CompositionTier::Extended => "extended",
                CompositionTier::VanillaEnhanced => "vanilla_enhanced",
            }),
            installed_count: state.map_or(0, |state| state.installed_mods.len()),
            warnings: inspection.warnings.clone(),
            managed_artifacts: health_response(inspection).managed_artifacts,
            operation: None,
        }
    }
}
