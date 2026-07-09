use super::PerformanceInstallResponse;
use super::mutation::execute_performance_operation;
use super::plan_health::{performance_artifacts_target, performance_composition_target};
use crate::guardian::GuardianPerformanceSupervisionPlan;
use crate::observability::{
    OperationProofRecord, RedactionAudience, operation_journal_proof_record,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStatus, OperationStepResult,
    OwnershipClass as StateOwnershipClass, RollbackState, StabilizationSystem, TargetDescriptor,
};
use crate::state::performance_operations::{
    PerformanceOperationConflict, PerformanceOperationPayload, PerformanceOperationStatus,
    sanitize_operation_error,
};
use crate::state::{AppState, DownloadProgress};
use axum::{Json, http::StatusCode};
use serde::Serialize;

const INVALID_PERSISTED_OPERATION_ERROR: &str = "invalid persisted performance operation payload";

#[derive(Debug, Serialize)]
pub struct PerformanceInstanceOperationResponse {
    pub operation: Option<PerformanceOperationStatusResponse>,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceOperationStatusResponse {
    #[serde(flatten)]
    pub status: PerformanceOperationStatus,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub proof: Option<OperationProofRecord>,
    pub view_model: PerformanceOperationStatusViewModel,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceOperationStatusViewModel {
    pub state_label: String,
    pub tone: &'static str,
    pub title: &'static str,
    pub detail: String,
    pub progress: PerformanceOperationProgressViewModel,
    pub is_terminal: bool,
    pub is_complete: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct PerformanceOperationProgressViewModel {
    pub phase: &'static str,
    pub current: u8,
    pub total: u8,
    pub done: bool,
}

#[derive(Debug, Clone)]
pub(super) struct PerformanceOperation {
    pub(super) instance_id: String,
    pub(super) game_version: Option<String>,
    pub(super) loader: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) action: PerformanceInstallAction,
    pub(super) rollback_id: Option<String>,
    pub(super) status_operation_id: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PerformanceInstallAction {
    Install,
    Remove,
    Rollback,
}

pub fn spawn_pending_performance_operations(state: &AppState) -> bool {
    let state = state.clone();
    tokio::spawn(async move {
        let resumed = resume_pending_performance_operations(state).await;
        if resumed > 0 {
            tracing::info!(
                resumed,
                "queued performance operations resumed after restart"
            );
        }
    });
    true
}

pub async fn performance_operation_status(
    state: &AppState,
    id: &str,
) -> Result<PerformanceOperationStatusResponse, (StatusCode, Json<serde_json::Value>)> {
    state
        .performance_operations()
        .get(id)
        .await
        .map(|status| public_performance_operation_status(state, status))
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "performance operation not found" })),
            )
        })
}

pub async fn performance_instance_operation(
    state: &AppState,
    instance_id: &str,
) -> Result<PerformanceInstanceOperationResponse, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let operation = state
        .performance_operations()
        .current_or_latest_for_instance(&instance.id)
        .await
        .map(|status| public_performance_operation_status(state, status));

    Ok(PerformanceInstanceOperationResponse { operation })
}

pub(super) async fn queue_performance_operation(
    state: AppState,
    mut operation: PerformanceOperation,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let status = state
        .performance_operations()
        .start(
            operation.instance_id.clone(),
            operation_action_name(operation.action).to_string(),
            operation_payload(&operation),
        )
        .await
        .map_err(performance_operation_conflict)?;
    let install_id = status.id.clone();
    operation.status_operation_id = Some(install_id.clone());
    state.installs().insert(install_id.clone()).await;

    let store = state.installs().clone();
    let install_id_task = install_id.clone();
    tokio::spawn(async move {
        run_queued_performance_operation(state, operation, store, install_id_task).await;
    });

    Ok(PerformanceInstallResponse {
        active: true,
        status: "queued".to_string(),
        install_id: Some(install_id),
        health: croopor_performance::BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
    })
}

pub(super) async fn resume_pending_performance_operations(state: AppState) -> usize {
    let pending = state
        .performance_operations()
        .take_pending_resumable_operations()
        .await;
    let resumed = pending.len();

    for status in pending {
        let install_id = status.id.clone();
        let operation = match operation_from_status(&status) {
            Ok(operation) => operation,
            Err(error) => {
                state
                    .performance_operations()
                    .record_failed(&install_id, &error)
                    .await;
                continue;
            }
        };
        state.installs().insert(install_id.clone()).await;
        let store = state.installs().clone();
        let state_task = state.clone();
        tokio::spawn(async move {
            run_queued_performance_operation(state_task, operation, store, install_id).await;
        });
    }

    resumed
}

async fn run_queued_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: String,
) {
    state
        .performance_operations()
        .record_progress(&install_id, "queued")
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        "queued",
        0,
        4,
        Some("Queued performance update"),
        None,
        false,
    )
    .await;
    state
        .performance_operations()
        .record_progress(&install_id, "planning")
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        "planning",
        1,
        4,
        Some("Planning performance bundle"),
        None,
        false,
    )
    .await;
    state
        .performance_operations()
        .record_progress(&install_id, operation_progress_phase(operation.action))
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        operation_progress_phase(operation.action),
        2,
        4,
        Some(operation_progress_label(operation.action)),
        None,
        false,
    )
    .await;

    match execute_performance_operation(&state, &operation).await {
        Ok(response) => {
            state
                .performance_operations()
                .record_complete(&install_id)
                .await;
            emit_performance_progress(
                &store,
                &install_id,
                "complete",
                4,
                4,
                Some(complete_progress_label(&response.status)),
                None,
                true,
            )
            .await;
        }
        Err(error) => {
            let message = error_message(&error);
            state
                .performance_operations()
                .record_failed(&install_id, &message)
                .await;
            emit_performance_progress(
                &store,
                &install_id,
                "error",
                4,
                4,
                None,
                Some(message),
                true,
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_performance_progress(
    store: &crate::state::InstallStore,
    install_id: &str,
    phase: &str,
    current: i32,
    total: i32,
    file: Option<&str>,
    error: Option<String>,
    done: bool,
) {
    store
        .emit(
            install_id,
            DownloadProgress {
                phase: phase.to_string(),
                current,
                total,
                file: file.map(ToOwned::to_owned),
                error,
                done,
                bytes_done: None,
                bytes_total: None,
            },
        )
        .await;
}

fn operation_progress_phase(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "applying",
        PerformanceInstallAction::Remove => "removing",
        PerformanceInstallAction::Rollback => "rolling_back",
    }
}

fn operation_progress_label(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "Applying managed performance files",
        PerformanceInstallAction::Remove => "Removing managed performance files",
        PerformanceInstallAction::Rollback => "Rolling back managed performance files",
    }
}

fn operation_action_name(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "install",
        PerformanceInstallAction::Remove => "remove",
        PerformanceInstallAction::Rollback => "rollback",
    }
}

fn operation_payload(operation: &PerformanceOperation) -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: operation.game_version.clone(),
        loader: operation.loader.clone(),
        mode: operation.mode.clone(),
        rollback_id: operation.rollback_id.clone(),
    }
}

fn public_performance_operation_status(
    state: &AppState,
    mut status: PerformanceOperationStatus,
) -> PerformanceOperationStatusResponse {
    let proof = performance_operation_proof(state, &status);
    status.instance_id = public_operation_required_token(&status.instance_id, "redacted");
    status.action = public_operation_required_token(&status.action, "unknown");
    status.state = public_operation_required_token(&status.state, "unknown");
    status.payload = public_operation_payload(status.payload);
    status.error = status
        .error
        .as_deref()
        .map(sanitize_operation_error)
        .filter(|value| !value.trim().is_empty());
    let view_model = performance_operation_view_model(&status);
    PerformanceOperationStatusResponse {
        status,
        proof,
        view_model,
    }
}

fn performance_operation_view_model(
    status: &PerformanceOperationStatus,
) -> PerformanceOperationStatusViewModel {
    let state = status.state.as_str();
    let failed = matches!(state, "failed" | "interrupted");
    let is_terminal = performance_status_is_terminal(state);
    let is_complete = state == "complete";
    let progress = PerformanceOperationProgressViewModel {
        phase: operation_status_progress_phase(state),
        current: operation_status_progress_current(state),
        total: 4,
        done: is_terminal,
    };

    PerformanceOperationStatusViewModel {
        state_label: public_state_label(state),
        tone: if failed {
            "err"
        } else if is_complete {
            "ok"
        } else {
            "mute"
        },
        title: operation_status_title(state),
        detail: if failed {
            status
                .error
                .clone()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "performance operation failed".to_string())
        } else {
            operation_status_detail(state).to_string()
        },
        progress,
        is_terminal,
        is_complete,
    }
}

fn operation_status_progress_phase(state: &str) -> &'static str {
    if matches!(state, "failed" | "interrupted") {
        "error"
    } else {
        match state {
            "queued" => "queued",
            "planning" => "planning",
            "applying" => "applying",
            "removing" => "removing",
            "rolling_back" => "rolling_back",
            "complete" => "complete",
            _ => "updating",
        }
    }
}

fn operation_status_progress_current(state: &str) -> u8 {
    match operation_status_progress_phase(state) {
        "queued" => 0,
        "planning" => 1,
        "complete" | "error" => 4,
        _ => 2,
    }
}

fn operation_status_title(state: &str) -> &'static str {
    match operation_status_progress_phase(state) {
        "queued" => "Bundle queued",
        "planning" => "Planning bundle",
        "applying" => "Applying bundle",
        "removing" => "Removing bundle",
        "rolling_back" => "Rolling back bundle",
        "complete" => "Bundle updated",
        "error" => "Bundle update failed",
        _ => "Updating bundle",
    }
}

fn operation_status_detail(state: &str) -> &'static str {
    match operation_status_progress_phase(state) {
        "queued" => "Waiting to update managed performance files.",
        "planning" => "Checking the managed performance plan.",
        "applying" => "Applying managed performance files.",
        "removing" => "Removing managed performance files.",
        "rolling_back" => "Rolling back managed performance files.",
        "complete" => "Managed performance update complete.",
        "error" => "Performance update failed.",
        _ => "Updating managed performance files.",
    }
}

fn public_state_label(state: &str) -> String {
    let labels = state
        .split(|character: char| !character.is_ascii_alphanumeric())
        .filter(|part| !part.is_empty())
        .map(|part| {
            let mut chars = part.chars();
            let Some(first) = chars.next() else {
                return String::new();
            };
            format!(
                "{}{}",
                first.to_ascii_uppercase(),
                chars.as_str().to_ascii_lowercase()
            )
        })
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>();

    if labels.is_empty() {
        "Unknown".to_string()
    } else {
        labels.join(" ")
    }
}

fn performance_operation_proof(
    state: &AppState,
    status: &PerformanceOperationStatus,
) -> Option<OperationProofRecord> {
    if !performance_status_is_terminal(&status.state) {
        return None;
    }
    let operation_id = OperationId::new(status.id.clone());
    state
        .journals()
        .get(&operation_id)
        .filter(|journal| performance_journal_is_terminal(journal.status))
        .map(|journal| operation_journal_proof_record(&journal))
}

fn performance_status_is_terminal(status: &str) -> bool {
    matches!(status, "complete" | "failed" | "interrupted")
}

fn performance_journal_is_terminal(status: OperationStatus) -> bool {
    matches!(
        status,
        OperationStatus::Succeeded
            | OperationStatus::Failed
            | OperationStatus::Blocked
            | OperationStatus::Cancelled
    )
}

fn public_operation_required_token(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn public_operation_payload(payload: PerformanceOperationPayload) -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: public_operation_payload_token(payload.game_version),
        loader: public_operation_payload_token(payload.loader),
        mode: public_operation_payload_token(payload.mode),
        rollback_id: public_operation_payload_token(payload.rollback_id),
    }
}

fn public_operation_payload_token(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
                .or_else(|| Some("redacted".to_string()))
        }
    })
}

fn operation_from_status(
    status: &PerformanceOperationStatus,
) -> Result<PerformanceOperation, String> {
    let action = operation_action_from_name(&status.action)
        .ok_or_else(|| INVALID_PERSISTED_OPERATION_ERROR.to_string())?;
    if status.instance_id.trim().is_empty() {
        return Err(INVALID_PERSISTED_OPERATION_ERROR.to_string());
    }

    Ok(PerformanceOperation {
        instance_id: status.instance_id.clone(),
        game_version: status.payload.game_version.clone(),
        loader: status.payload.loader.clone(),
        mode: status.payload.mode.clone(),
        action,
        rollback_id: status.payload.rollback_id.clone(),
        status_operation_id: Some(status.id.clone()),
    })
}

fn operation_action_from_name(action: &str) -> Option<PerformanceInstallAction> {
    match action {
        "install" => Some(PerformanceInstallAction::Install),
        "remove" => Some(PerformanceInstallAction::Remove),
        "rollback" => Some(PerformanceInstallAction::Rollback),
        _ => None,
    }
}

fn complete_progress_label(status: &str) -> &'static str {
    match status {
        "removed" => "Managed performance files removed",
        "rolled_back" => "Managed performance files rolled back",
        _ => "Managed performance bundle updated",
    }
}

fn error_message(error: &(StatusCode, Json<serde_json::Value>)) -> String {
    error
        .1
        .0
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("performance operation failed")
        .to_string()
}

pub(super) fn begin_performance_operation_journal(
    state: &AppState,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
    linked_operation_id: Option<&str>,
) -> OperationId {
    let operation_id = linked_operation_id
        .map(OperationId::new)
        .unwrap_or_else(|| generated_performance_journal_operation_id(action));
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::ApplyPerformancePlan,
        StabilizationSystem::Application,
        StateOwnershipClass::CompositionManaged,
        rollback,
    );
    entry.targets = performance_operation_targets(target_id);
    entry.planned_steps.push(performance_operation_journal_step(
        action,
        OperationStepResult::Planned,
        target_id,
        rollback,
    ));
    state.journals().create(entry);
    operation_id
}

fn generated_performance_journal_operation_id(action: PerformanceInstallAction) -> OperationId {
    OperationId::new(format!(
        "performance-{}-{}",
        performance_operation_step_id(action),
        uuid::Uuid::new_v4()
    ))
}

pub(super) fn record_performance_operation_result(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    fallback_target_id: &str,
    rollback: RollbackState,
    result: &Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)>,
) {
    match result {
        Ok(response) => {
            let response_target_id = response.composition_id.trim();
            let target_id = if response_target_id.is_empty() {
                fallback_target_id
            } else {
                response_target_id
            };
            record_performance_operation_success(state, operation_id, action, target_id, rollback);
        }
        Err(_) => record_performance_operation_failure(
            state,
            operation_id,
            action,
            fallback_target_id,
            rollback,
        ),
    }
}

fn record_performance_operation_success(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) {
    let rollback = if matches!(action, PerformanceInstallAction::Rollback) {
        RollbackState::Applied
    } else {
        rollback
    };
    state.journals().record_success(
        operation_id,
        performance_operation_journal_step(
            action,
            OperationStepResult::Completed,
            target_id,
            rollback,
        ),
        OperationOutcome::Succeeded,
    );
}

pub(super) fn record_performance_operation_failure(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) {
    state.journals().record_failure(
        operation_id,
        performance_operation_journal_step(
            action,
            OperationStepResult::Failed,
            target_id,
            rollback,
        ),
        performance_operation_step_id(action),
        OperationOutcome::Failed,
    );
}

pub(super) fn record_performance_guardian_supervision(
    state: &AppState,
    operation_id: &OperationId,
    supervision: &GuardianPerformanceSupervisionPlan,
) {
    state.journals().record_guardian_evidence(
        operation_id,
        supervision.fact_ids.clone(),
        supervision
            .decision
            .diagnoses
            .iter()
            .map(|diagnosis| diagnosis.as_str().to_string())
            .collect(),
    );
}

fn performance_operation_journal_step(
    action: PerformanceInstallAction,
    result: OperationStepResult,
    target_id: &str,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(
        performance_operation_step_id(action),
        performance_operation_phase(action),
    );
    step.result = result;
    step.rollback = rollback;
    step.generated_facts
        .push("performance_operation_evidence".to_string());
    if !matches!(rollback, RollbackState::NotApplicable) {
        step.generated_facts
            .push("performance_rollback_evidence".to_string());
    }
    if result != OperationStepResult::Planned {
        step.changed_target = Some(performance_composition_target(target_id));
    }
    step
}

fn performance_operation_targets(target_id: &str) -> Vec<TargetDescriptor> {
    vec![
        performance_composition_target(target_id),
        performance_artifacts_target(target_id),
    ]
}

fn performance_operation_step_id(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "apply_performance_plan",
        PerformanceInstallAction::Remove => "remove_performance_plan",
        PerformanceInstallAction::Rollback => "rollback_performance_plan",
    }
}

fn performance_operation_phase(action: PerformanceInstallAction) -> OperationPhase {
    match action {
        PerformanceInstallAction::Install | PerformanceInstallAction::Remove => {
            OperationPhase::Installing
        }
        PerformanceInstallAction::Rollback => OperationPhase::RollingBack,
    }
}

pub(super) fn install_action(
    raw: Option<&str>,
) -> Result<PerformanceInstallAction, (StatusCode, Json<serde_json::Value>)> {
    match super::optional_value(raw).as_deref() {
        None | Some("install") | Some("apply") => Ok(PerformanceInstallAction::Install),
        Some("remove") | Some("disable") => Ok(PerformanceInstallAction::Remove),
        Some("rollback") => Ok(PerformanceInstallAction::Rollback),
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid performance action" })),
        )),
    }
}

fn performance_operation_conflict(
    _error: PerformanceOperationConflict,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "a performance operation is already queued for this instance"
        })),
    )
}
