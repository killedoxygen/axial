use crate::state::{AppState, RequestProducerHandoff};
use axial_performance::BundleHealth;
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};

mod mutation;
mod operations;
mod plan_health;

#[cfg(test)]
use crate::state::contracts::RollbackState;
#[cfg(test)]
use axial_performance::{CompositionTier, InstallError, PerformanceMode};
#[cfg(test)]
use mutation::{
    PERFORMANCE_INSTALL_INTERNAL_ERROR, execute_performance_operation, performance_install_error,
};
pub use mutation::{PerformanceRollbackListResponse, performance_rollback_list};

pub(crate) use operations::spawn_pending_performance_operations;
#[cfg(test)]
use operations::{
    PERFORMANCE_JOURNAL_ERROR, PerformanceInstallAction, PerformanceOperationExecutionError,
    PerformanceWorkerIdentity, begin_performance_operation_journal,
    performance_journal_is_terminal, record_performance_effect_started,
    record_performance_terminal_intent, retry_performance_status_correction,
    retry_performance_status_transition, run_queued_performance_operation,
    stage_performance_installed_versions, supervise_performance_worker,
    terminalize_mismatched_performance_operation,
};
pub use operations::{
    PerformanceInstanceOperationResponse, PerformanceOperationStatusResponse,
    performance_instance_operation, performance_operation_status,
};
use operations::{
    PerformanceOperation, execute_synchronous_performance_operation, install_action,
    queue_performance_operation,
};

#[cfg(test)]
async fn resume_pending_performance_operations(state: AppState) -> usize {
    let producer = state
        .try_claim_producer()
        .expect("claim test performance resume producer");
    let child_owner = producer.claim_child();
    let shutdown = state.subscribe_shutdown();
    operations::resume_pending_performance_operations_owned(state, &child_owner, shutdown).await
}
pub(crate) use plan_health::performance_health;
#[cfg(test)]
use plan_health::{
    PERFORMANCE_DATA_INTERNAL_ERROR, bundle_health_token, internal_error,
    resolve_instance_version_target,
};
pub use plan_health::{
    PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstanceDisplay,
    PerformanceManagedArtifactSummary, PerformanceMemoryDisplay, PerformanceModeDisplay,
    PerformancePlanRequest, PerformancePlanResponse, PerformanceRuntimeDisplay, performance_plan,
};

#[derive(Debug, Deserialize)]
pub struct PerformanceRollbackListRequest {
    pub instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PerformanceInstallRequest {
    pub instance_id: Option<String>,
    pub game_version: Option<String>,
    pub loader: Option<String>,
    pub mode: Option<String>,
    pub action: Option<String>,
    pub rollback_id: Option<String>,
    pub queued: Option<bool>,
}

#[derive(Debug, Serialize)]
pub struct PerformanceInstallResponse {
    pub active: bool,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub install_id: Option<String>,
    pub health: BundleHealth,
    pub composition_id: String,
    pub tier: String,
    pub installed_count: usize,
    pub managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    pub warnings: Vec<String>,
}

pub(crate) async fn performance_install(
    state: AppState,
    payload: PerformanceInstallRequest,
    handoff: RequestProducerHandoff,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(payload.instance_id.as_deref(), "instance_id is required")?;
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let action = install_action(payload.action.as_deref())?;
    let operation = PerformanceOperation {
        instance_id: instance.id.clone(),
        game_version: payload.game_version.clone(),
        loader: payload.loader.clone(),
        mode: payload.mode.clone(),
        action,
        rollback_id: payload.rollback_id.clone(),
        status_operation_id: None,
        persistence_failure: None,
        installed_versions: None,
    };

    if payload.queued.unwrap_or(false) {
        return queue_performance_operation(state, operation, handoff).await;
    }

    execute_synchronous_performance_operation(state, operation, handoff).await
}

fn required_value(
    raw: Option<&str>,
    message: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    optional_value(raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
    })
}

fn optional_value(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

#[cfg(test)]
mod tests;
