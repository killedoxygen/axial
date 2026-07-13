use super::PerformanceInstallResponse;
use super::mutation::{execute_performance_operation, performance_operation_journal_identity};
use super::plan_health::{performance_artifacts_target, performance_composition_target};
use crate::guardian::{DiagnosisId, GuardianPerformanceSupervisionPlan};
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
    PERFORMANCE_COMMITTING_COMPLETE_STATE, PERFORMANCE_COMMITTING_FAILED_STATE,
    PERFORMANCE_EFFECT_STARTED_STATE, PerformanceOperationJournalIdentity,
    PerformanceOperationPayload, PerformanceOperationStartError, PerformanceOperationStatus,
    PerformanceOperationStoreError, normalized_operation_timestamp, sanitize_operation_error,
};
use crate::state::{
    AppState, DownloadProgress, InstalledVersionsSnapshot, IntegrityForegroundLease,
    IntegrityForegroundRegistration, OperationJournalStoreError, ProducerLease,
    RequestProducerHandoff,
};
use axum::{Json, http::StatusCode};
use serde::Serialize;
use std::{
    collections::HashSet,
    future::Future,
    sync::{Arc, Mutex},
};

const INVALID_PERSISTED_OPERATION_ERROR: &str = "invalid persisted performance operation payload";
pub(super) const PERFORMANCE_JOURNAL_ERROR: &str =
    "Could not save performance operation safety state. Check app data permissions and try again.";
const PERFORMANCE_EFFECT_GATE_FACT: &str = "performance_effect_gate_v1";
const PERFORMANCE_EFFECT_STARTED_FACT: &str = "performance_effect_started_v1";
const PERFORMANCE_TERMINAL_SUCCESS_FACT: &str = "performance_terminal_success_v1";
const PERFORMANCE_TERMINAL_FAILURE_FACT: &str = "performance_terminal_failure_v1";
const PERFORMANCE_INVALID_JOURNAL_FAILURE_POINT: &str = "performance_journal_invalid";
const PERFORMANCE_RECONCILIATION_FAILURE: &str =
    "performance operation outcome could not be confirmed after restart";
const PERFORMANCE_WORKER_INTERRUPTED_FAILURE: &str =
    "performance operation stopped before its result could be confirmed";
const PERFORMANCE_RETRY_INITIAL_DELAY_MS: u64 = 20;
const PERFORMANCE_RETRY_MAX_DELAY_MS: u64 = 1_000;

pub(super) type PerformanceApplicationError = (StatusCode, Json<serde_json::Value>);

fn performance_shutdown_error() -> PerformanceApplicationError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "performance operations are shutting down"
        })),
    )
}

#[derive(Debug)]
pub(super) enum PerformanceOperationExecutionError {
    Operation(PerformanceApplicationError),
    Journal {
        error: OperationJournalStoreError,
        operation_id: Option<OperationId>,
        expected: Option<PerformanceJournalTransition>,
    },
    Status(PerformanceOperationStoreError),
}

impl PerformanceOperationExecutionError {
    pub(super) fn journal_transition(
        operation_id: Option<OperationId>,
        error: OperationJournalStoreError,
        expected: PerformanceJournalTransition,
    ) -> Self {
        Self::Journal {
            error,
            operation_id,
            expected: Some(expected),
        }
    }

    #[cfg(test)]
    pub(super) fn into_application_error(self) -> PerformanceApplicationError {
        match self {
            Self::Operation(error) => error,
            Self::Journal { .. } | Self::Status(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
            ),
        }
    }
}

impl From<PerformanceApplicationError> for PerformanceOperationExecutionError {
    fn from(error: PerformanceApplicationError) -> Self {
        Self::Operation(error)
    }
}

impl From<PerformanceOperationStoreError> for PerformanceOperationExecutionError {
    fn from(error: PerformanceOperationStoreError) -> Self {
        Self::Status(error)
    }
}

#[derive(Clone, Debug)]
pub(super) enum PerformanceJournalTransition {
    Created {
        action: PerformanceInstallAction,
        target_id: String,
        rollback: RollbackState,
    },
    GuardianEvidence {
        action: PerformanceInstallAction,
        target_id: String,
        rollback: RollbackState,
        fact_ids: Vec<String>,
        diagnosis_ids: Vec<DiagnosisId>,
    },
    EffectStarted {
        action: PerformanceInstallAction,
        target_id: String,
        rollback: RollbackState,
    },
    TerminalIntent {
        action: PerformanceInstallAction,
        base_target_id: String,
        result_target_id: String,
        rollback: RollbackState,
        succeeded: bool,
    },
    Terminal {
        action: PerformanceInstallAction,
        base_target_id: String,
        result_target_id: String,
        rollback: RollbackState,
        succeeded: bool,
    },
}

impl PerformanceJournalTransition {
    pub(super) fn created(
        action: PerformanceInstallAction,
        target_id: &str,
        rollback: RollbackState,
    ) -> Self {
        Self::Created {
            action,
            target_id: target_id.to_string(),
            rollback,
        }
    }

    pub(super) fn guardian(
        action: PerformanceInstallAction,
        target_id: &str,
        rollback: RollbackState,
        supervision: &GuardianPerformanceSupervisionPlan,
    ) -> Self {
        Self::GuardianEvidence {
            action,
            target_id: target_id.to_string(),
            rollback,
            fact_ids: supervision
                .fact_ids
                .iter()
                .map(|fact_id| fact_id.as_str().to_string())
                .collect(),
            diagnosis_ids: supervision.decision.diagnoses().to_vec(),
        }
    }

    pub(super) fn effect_started(
        action: PerformanceInstallAction,
        target_id: &str,
        rollback: RollbackState,
    ) -> Self {
        Self::EffectStarted {
            action,
            target_id: target_id.to_string(),
            rollback,
        }
    }

    pub(super) fn terminal_intent(
        action: PerformanceInstallAction,
        base_target_id: &str,
        result_target_id: &str,
        rollback: RollbackState,
        succeeded: bool,
    ) -> Self {
        Self::TerminalIntent {
            action,
            base_target_id: base_target_id.to_string(),
            result_target_id: result_target_id.to_string(),
            rollback,
            succeeded,
        }
    }

    pub(super) fn terminal(
        action: PerformanceInstallAction,
        base_target_id: &str,
        result_target_id: &str,
        rollback: RollbackState,
        succeeded: bool,
    ) -> Self {
        Self::Terminal {
            action,
            base_target_id: base_target_id.to_string(),
            result_target_id: result_target_id.to_string(),
            rollback,
            succeeded,
        }
    }

    fn matches(&self, journal: &OperationJournalEntry) -> bool {
        match self {
            Self::Created {
                action,
                target_id,
                rollback,
            } => performance_journal_matches(journal, *action, target_id, *rollback),
            Self::GuardianEvidence {
                action,
                target_id,
                rollback,
                fact_ids,
                diagnosis_ids,
            } => {
                performance_journal_matches(journal, *action, target_id, *rollback)
                    && fact_ids.iter().all(|fact_id| {
                        journal.completed_steps.iter().any(|step| {
                            step.generated_facts
                                .iter()
                                .any(|candidate| candidate == fact_id)
                        })
                    })
                    && diagnosis_ids.iter().all(|diagnosis_id| {
                        journal
                            .guardian_diagnosis_ids
                            .iter()
                            .any(|candidate| candidate == diagnosis_id)
                    })
            }
            Self::EffectStarted {
                action,
                target_id,
                rollback,
            } => {
                performance_journal_matches(journal, *action, target_id, *rollback)
                    && journal.completed_steps.iter().any(|step| {
                        step.step_id == "performance_effect_started"
                            && step.result == OperationStepResult::Completed
                            && step.rollback == *rollback
                            && step.changed_target.is_none()
                            && step
                                .generated_facts
                                .iter()
                                .any(|fact| fact == PERFORMANCE_EFFECT_STARTED_FACT)
                    })
            }
            Self::TerminalIntent {
                action,
                base_target_id,
                result_target_id,
                rollback,
                succeeded,
            } => performance_terminal_intent(journal).is_some_and(|intent| {
                intent.action == *action
                    && intent.base_target_id == *base_target_id
                    && intent.result_target_id == *result_target_id
                    && intent.rollback == *rollback
                    && intent.succeeded == *succeeded
            }),
            Self::Terminal {
                action,
                base_target_id,
                result_target_id,
                rollback,
                succeeded,
            } => performance_terminal_transition(journal).is_some_and(|intent| {
                intent.action == *action
                    && intent.base_target_id == *base_target_id
                    && intent.result_target_id == *result_target_id
                    && intent.rollback == *rollback
                    && intent.succeeded == *succeeded
            }),
        }
    }
}

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

#[derive(Clone)]
pub(super) struct PerformanceOperation {
    pub(super) instance_id: String,
    pub(super) game_version: Option<String>,
    pub(super) loader: Option<String>,
    pub(super) mode: Option<String>,
    pub(super) action: PerformanceInstallAction,
    pub(super) rollback_id: Option<String>,
    pub(super) status_operation_id: Option<String>,
    pub(super) persistence_failure: Option<PerformancePersistenceFailureSignal>,
    pub(super) installed_versions: Option<InstalledVersionsSnapshot>,
}

#[derive(Clone, Debug)]
pub(super) struct PerformancePersistenceFailureSignal {
    sender: std::sync::Arc<std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>>,
}

impl PerformancePersistenceFailureSignal {
    fn new() -> (Self, tokio::sync::oneshot::Receiver<()>) {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        (
            Self {
                sender: std::sync::Arc::new(std::sync::Mutex::new(Some(sender))),
            },
            receiver,
        )
    }

    pub(super) fn notify(&self) {
        if let Some(sender) = self
            .sender
            .lock()
            .expect("performance persistence failure signal lock poisoned")
            .take()
        {
            let _ = sender.send(());
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PerformanceInstallAction {
    Install,
    Remove,
    Rollback,
}

#[derive(Clone, Default)]
pub(super) struct PerformanceWorkerIdentity {
    operation_id: Arc<Mutex<Option<String>>>,
}

impl PerformanceWorkerIdentity {
    pub(super) fn set(&self, operation_id: &str) {
        *self
            .operation_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(operation_id.to_string());
    }

    fn get(&self) -> Option<String> {
        self.operation_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

fn register_performance_foreground(
    state: &AppState,
) -> Result<IntegrityForegroundRegistration, PerformanceApplicationError> {
    state
        .register_integrity_foreground()
        .map_err(|_| performance_shutdown_error())
}

pub(super) async fn supervise_performance_worker<F, Fut>(
    state: AppState,
    action: PerformanceInstallAction,
    identity: PerformanceWorkerIdentity,
    supervisor_owner: ProducerLease,
    foreground: IntegrityForegroundLease,
    worker: F,
) where
    F: FnOnce(ProducerLease, IntegrityForegroundLease) -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let worker_owner = supervisor_owner.claim_child();
    let runtime_owner = worker_owner.claim_child();
    let worker_foreground = foreground.retained();
    let worker = worker_owner.spawn_joinable(worker(runtime_owner, worker_foreground));
    let worker_result = worker.await;
    let interrupted = worker_result.is_err();
    if let Err(error) = worker_result {
        tracing::error!(
            worker_cancelled = error.is_cancelled(),
            worker_panicked = error.is_panic(),
            "performance operation worker stopped before settlement"
        );
    }

    let Some(install_id) = identity.get() else {
        return;
    };
    if !state
        .performance_operations()
        .has_reconciliation_obligation(&install_id)
    {
        return;
    }
    let status = state.performance_operations().get(&install_id).await;
    if !interrupted
        && status
            .as_ref()
            .is_some_and(|status| performance_status_is_terminal(&status.state))
    {
        return;
    }

    let mut shutdown = state.subscribe_shutdown();
    let mut integrity = state.subscribe_integrity_idle();
    let mut delay_ms = PERFORMANCE_RETRY_INITIAL_DELAY_MS;
    loop {
        if !state
            .performance_operations()
            .has_reconciliation_obligation(&install_id)
        {
            return;
        }
        if state
            .performance_operations()
            .get(&install_id)
            .await
            .is_none()
            && state
                .performance_operations()
                .has_retry_candidate(&install_id)
        {
            let operation_id = OperationId::new(install_id.clone());
            let _ = retry_performance_status_transition(
                &state,
                &operation_id,
                "queued",
                None,
                Err(PerformanceOperationStoreError::RetryRequired),
                None,
            )
            .await;
        }
        state.installs().insert(install_id.clone()).await;
        let store = state.installs().clone();
        let terminalization = terminalize_uncertain_performance_operation(
            &state,
            &store,
            &install_id,
            Some(action),
            PERFORMANCE_WORKER_INTERRUPTED_FAILURE,
            None,
        );
        tokio::pin!(terminalization);
        let published = loop {
            tokio::select! {
                published = &mut terminalization => break published,
                _ = shutdown.changed() => {},
                _ = integrity.changed() => {},
            }
            if *shutdown.borrow_and_update() && !integrity.borrow_and_update().is_running() {
                tracing::warn!(
                    operation_id = install_id,
                    "performance terminal supervision stopped after integrity shutdown"
                );
                return;
            }
        };
        let terminal = state
            .performance_operations()
            .get(&install_id)
            .await
            .is_some_and(|status| performance_status_is_terminal(&status.state));
        if terminal {
            return;
        }
        tracing::error!(
            operation_id = install_id,
            published,
            "performance worker supervision is retaining authority for terminal retry"
        );
        if *shutdown.borrow_and_update() && !integrity.borrow_and_update().is_running() {
            tracing::warn!(
                operation_id = install_id,
                "performance terminal supervision stopped after integrity shutdown"
            );
            return;
        }
        tokio::select! {
            _ = tokio::time::sleep(std::time::Duration::from_millis(delay_ms)) => {}
            _ = shutdown.changed() => {}
            _ = integrity.changed() => {}
        }
        delay_ms = delay_ms
            .saturating_mul(2)
            .min(PERFORMANCE_RETRY_MAX_DELAY_MS);
    }
}

pub(crate) fn spawn_pending_performance_operations(state: &AppState, producer: ProducerLease) {
    let state = state.clone();
    let child_owner = producer.claim_child();
    let shutdown = state.subscribe_shutdown();
    producer.spawn(async move {
        let resumed =
            resume_pending_performance_operations_owned(state, &child_owner, shutdown).await;
        if resumed > 0 {
            tracing::info!(
                resumed,
                "queued performance operations resumed after restart"
            );
        }
    });
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
    operation: PerformanceOperation,
    handoff: RequestProducerHandoff,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ownership_tx, ownership_rx) = tokio::sync::oneshot::channel();
    let producer = handoff
        .try_claim()
        .map_err(|_| performance_shutdown_error())?;
    let foreground = register_performance_foreground(&state)?;
    let worker_owner = producer.claim_child();
    let operation_id = state.performance_operations().reserve_operation_id();
    let worker_identity = PerformanceWorkerIdentity::default();
    worker_identity.set(operation_id.operation_id());
    let supervisor_identity = worker_identity.clone();
    let action = operation.action;
    let supervisor_state = state.clone();
    producer.spawn(async move {
        let foreground = foreground.wait_for_settlement().await;
        supervise_performance_worker(
            supervisor_state,
            action,
            supervisor_identity,
            worker_owner,
            foreground,
            move |runtime_owner, worker_foreground| async move {
                let mut operation = operation;
                operation.installed_versions = stage_performance_installed_versions(
                    &state,
                    &operation,
                    &runtime_owner,
                    &worker_foreground,
                )
                .await;
                let journal_identity =
                    durable_performance_operation_identity(&state, &operation, &worker_foreground)
                        .await;
                let status = match state
                    .performance_operations()
                    .start_reserved_with_identity(
                        operation_id,
                        operation.instance_id.clone(),
                        operation_action_name(operation.action).to_string(),
                        operation_payload(&operation),
                        journal_identity,
                    )
                    .await
                {
                    Ok(status) => status,
                    Err(error) => {
                        let retry_operation_id = error.operation_id().and_then(|operation_id| {
                            state
                                .performance_operations()
                                .has_retry_candidate(operation_id)
                                .then(|| operation_id.to_string())
                        });
                        if let Some(install_id) = retry_operation_id.as_deref() {
                            worker_identity.set(install_id);
                        }
                        let _ = ownership_tx.send(Err(performance_operation_start_error(error)));
                        if let Some(install_id) = retry_operation_id {
                            reconcile_failed_performance_start(&state, &operation, &install_id)
                                .await;
                        }
                        return;
                    }
                };
                let install_id = status.id.clone();
                worker_identity.set(&install_id);
                operation.status_operation_id = Some(install_id.clone());
                state.installs().insert(install_id.clone()).await;
                let store = state.installs().clone();
                let response = PerformanceInstallResponse {
                    active: true,
                    status: "queued".to_string(),
                    install_id: Some(install_id.clone()),
                    health: axial_performance::BundleHealth::Disabled,
                    composition_id: String::new(),
                    tier: String::new(),
                    installed_count: 0,
                    managed_artifacts: Vec::new(),
                    warnings: Vec::new(),
                };
                let _ = ownership_tx.send(Ok(response));
                run_queued_performance_operation(
                    state,
                    operation,
                    store,
                    install_id,
                    worker_foreground,
                )
                .await;
            },
        )
        .await;
    });

    ownership_rx.await.unwrap_or_else(|_| {
        Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
        ))
    })
}

pub(super) async fn execute_synchronous_performance_operation(
    state: AppState,
    mut operation: PerformanceOperation,
    handoff: RequestProducerHandoff,
) -> Result<PerformanceInstallResponse, PerformanceApplicationError> {
    let (completion_tx, completion_rx) = tokio::sync::oneshot::channel();
    let (failure_signal, failure_rx) = PerformancePersistenceFailureSignal::new();
    operation.persistence_failure = Some(failure_signal);
    let producer = handoff
        .try_claim()
        .map_err(|_| performance_shutdown_error())?;
    let foreground = register_performance_foreground(&state)?;
    let worker_owner = producer.claim_child();
    let operation_id = state.performance_operations().reserve_operation_id();
    let worker_identity = PerformanceWorkerIdentity::default();
    worker_identity.set(operation_id.operation_id());
    let supervisor_identity = worker_identity.clone();
    let action = operation.action;
    let supervisor_state = state.clone();
    producer.spawn(async move {
        let foreground = foreground.wait_for_settlement().await;
        supervise_performance_worker(
            supervisor_state,
            action,
            supervisor_identity,
            worker_owner,
            foreground,
            move |runtime_owner, worker_foreground| async move {
                operation.installed_versions = stage_performance_installed_versions(
                    &state,
                    &operation,
                    &runtime_owner,
                    &worker_foreground,
                )
                .await;
                let journal_identity =
                    durable_performance_operation_identity(&state, &operation, &worker_foreground)
                        .await;
                let status = match state
                    .performance_operations()
                    .start_reserved_with_identity(
                        operation_id,
                        operation.instance_id.clone(),
                        operation_action_name(operation.action).to_string(),
                        operation_payload(&operation),
                        journal_identity,
                    )
                    .await
                {
                    Ok(status) => status,
                    Err(error) => {
                        let retry_operation_id = error.operation_id().and_then(|operation_id| {
                            state
                                .performance_operations()
                                .has_retry_candidate(operation_id)
                                .then(|| operation_id.to_string())
                        });
                        if let Some(install_id) = retry_operation_id.as_deref() {
                            worker_identity.set(install_id);
                        }
                        let _ = completion_tx.send(Err(performance_operation_start_error(error)));
                        if let Some(install_id) = retry_operation_id {
                            reconcile_failed_performance_start(&state, &operation, &install_id)
                                .await;
                        }
                        return;
                    }
                };
                let install_id = status.id.clone();
                worker_identity.set(&install_id);
                operation.status_operation_id = Some(install_id.clone());
                state.installs().insert(install_id.clone()).await;
                let store = state.installs().clone();
                run_owned_performance_operation(
                    state,
                    operation,
                    store,
                    install_id,
                    Some(completion_tx),
                    &worker_foreground,
                )
                .await;
            },
        )
        .await;
    });

    let mut completion_rx = completion_rx;
    tokio::select! {
        result = &mut completion_rx => result.unwrap_or_else(|_| Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
        ))),
        failure = failure_rx => match failure {
            Ok(()) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
            )),
            Err(_) => completion_rx.await.unwrap_or_else(|_| Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
            ))),
        },
    }
}

async fn reconcile_failed_performance_start(
    state: &AppState,
    operation: &PerformanceOperation,
    install_id: &str,
) {
    let operation_id = OperationId::new(install_id.to_string());
    if retry_performance_status_transition(
        state,
        &operation_id,
        "queued",
        None,
        Err(PerformanceOperationStoreError::RetryRequired),
        None,
    )
    .await
    .is_ok()
    {
        state.installs().insert(install_id.to_string()).await;
        let store = state.installs().clone();
        terminalize_uncertain_performance_operation(
            state,
            &store,
            install_id,
            Some(operation.action),
            PERFORMANCE_JOURNAL_ERROR,
            None,
        )
        .await;
    }
}

pub(super) async fn resume_pending_performance_operations_owned(
    state: AppState,
    producer: &ProducerLease,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> usize {
    let reconciliation = plan_orphaned_performance_reconciliation(&state);
    if reconciliation.is_empty()
        && !state
            .performance_operations()
            .has_pending_resumable_operations()
    {
        return 0;
    }
    let Ok(foreground) = state.register_integrity_foreground() else {
        return 0;
    };
    let foreground = foreground.wait_for_settlement().await;
    let mut resumed = 0usize;
    reconcile_orphaned_performance_journals(&state, reconciliation).await;
    loop {
        if *shutdown.borrow_and_update() {
            break;
        }
        let pending = state
            .performance_operations()
            .take_pending_resumable_operations()
            .await;
        if pending.is_empty() {
            break;
        }
        for status in pending {
            if *shutdown.borrow_and_update() {
                return resumed;
            }
            resumed = resumed.saturating_add(1);
            let install_id = status.id.clone();
            state.installs().insert(install_id.clone()).await;
            let store = state.installs().clone();
            if let Some(mut operation) =
                prepare_resumed_performance_operation(&state, &status, &store).await
            {
                let action = operation.action;
                let worker_identity = PerformanceWorkerIdentity::default();
                worker_identity.set(&install_id);
                let supervisor_identity = worker_identity.clone();
                let worker_owner = producer.claim_child();
                let worker_foreground = foreground.retained();
                let state_task = state.clone();
                let supervisor_state = state.clone();
                producer.spawn_child(async move {
                    supervise_performance_worker(
                        supervisor_state,
                        action,
                        supervisor_identity,
                        worker_owner,
                        worker_foreground,
                        move |runtime_owner, operation_foreground| async move {
                            operation.installed_versions = stage_performance_installed_versions(
                                &state_task,
                                &operation,
                                &runtime_owner,
                                &operation_foreground,
                            )
                            .await;
                            run_queued_performance_operation(
                                state_task,
                                operation,
                                store,
                                install_id,
                                operation_foreground,
                            )
                            .await;
                        },
                    )
                    .await;
                });
            }
        }
    }

    resumed
}

pub(super) async fn stage_performance_installed_versions(
    state: &AppState,
    operation: &PerformanceOperation,
    producer: &ProducerLease,
    foreground: &IntegrityForegroundLease,
) -> Option<InstalledVersionsSnapshot> {
    if !matches!(operation.action, PerformanceInstallAction::Install) {
        return None;
    }
    state
        .installed_versions_snapshot_with_foreground(producer, foreground.retained())
        .await
        .map(|lookup| lookup.snapshot)
}

#[derive(Default)]
struct PerformanceReconciliationPlan {
    mismatched_statuses: Vec<(PerformanceOperationStatus, PerformanceInstallAction)>,
    orphaned_journals: Vec<OperationJournalEntry>,
}

impl PerformanceReconciliationPlan {
    fn is_empty(&self) -> bool {
        self.mismatched_statuses.is_empty() && self.orphaned_journals.is_empty()
    }
}

fn plan_orphaned_performance_reconciliation(state: &AppState) -> PerformanceReconciliationPlan {
    let statuses = state.performance_operations().list();
    let mut mismatched_statuses = Vec::new();
    for status in statuses
        .iter()
        .filter(|status| performance_status_is_terminal(&status.state))
    {
        if performance_status_has_mismatch_reconciliation(state, status) {
            continue;
        }
        let operation_id = OperationId::new(status.id.clone());
        let Some(journal) = state.journals().get(&operation_id) else {
            continue;
        };
        if journal.command == CommandKind::ApplyPerformancePlan
            && !performance_terminal_matches_status(&journal, status)
        {
            let action = status
                .journal_identity
                .as_ref()
                .and_then(|identity| operation_action_from_name(&identity.action))
                .or_else(|| operation_action_from_name(&status.action))
                .unwrap_or(PerformanceInstallAction::Install);
            mismatched_statuses.push((status.clone(), action));
        }
    }

    let nonterminal_status_ids = statuses
        .into_iter()
        .filter(|status| !performance_status_is_terminal(&status.state))
        .map(|status| status.id)
        .collect::<HashSet<_>>();
    let orphaned_journals = state
        .journals()
        .list()
        .into_iter()
        .filter(|journal| {
            journal.command == CommandKind::ApplyPerformancePlan
                && !performance_journal_is_terminal(journal.status)
        })
        .filter(|journal| !nonterminal_status_ids.contains(journal.operation_id.as_str()))
        .collect::<Vec<_>>();

    PerformanceReconciliationPlan {
        mismatched_statuses,
        orphaned_journals,
    }
}

async fn reconcile_orphaned_performance_journals(
    state: &AppState,
    plan: PerformanceReconciliationPlan,
) {
    for (status, action) in plan.mismatched_statuses {
        terminalize_mismatched_performance_operation(
            state,
            state.installs(),
            &status,
            action,
            PERFORMANCE_RECONCILIATION_FAILURE,
        )
        .await;
    }

    for journal in plan.orphaned_journals {
        if let Err(error) = terminalize_orphaned_performance_journal(state, &journal).await {
            tracing::warn!(
                operation_id = journal.operation_id.as_str(),
                journal_error = error.class(),
                "orphaned performance journal reconciliation was rejected"
            );
        }
    }
}

async fn terminalize_orphaned_performance_journal(
    state: &AppState,
    journal: &OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    let Some(identity) = performance_journal_identity(journal) else {
        return terminalize_invalid_performance_journal(state, journal).await;
    };
    let action = identity.action;
    let base_target_id = identity.target_id;
    let intent = match performance_terminal_intent(journal) {
        Some(intent) => intent,
        None if performance_journal_has_terminal_marker(journal) => {
            return terminalize_invalid_performance_journal(state, journal).await;
        }
        None => {
            let expected = PerformanceJournalTransition::terminal_intent(
                action,
                &base_target_id,
                &base_target_id,
                journal.rollback,
                false,
            );
            loop {
                match record_performance_terminal_intent(
                    state,
                    &journal.operation_id,
                    action,
                    &base_target_id,
                    journal.rollback,
                    false,
                )
                .await
                {
                    Ok(()) => break,
                    Err(OperationJournalStoreError::AlreadyTerminal)
                        if state
                            .journals()
                            .get(&journal.operation_id)
                            .as_ref()
                            .is_some_and(|entry| expected.matches(entry)) =>
                    {
                        break;
                    }
                    Err(error) => match reconcile_performance_journal_transition(
                        state,
                        &journal.operation_id,
                        error,
                        &expected,
                    )
                    .await?
                    {
                        JournalRetryOutcome::RequestedTransitionCommitted => break,
                        JournalRetryOutcome::RetryRequestedTransition => continue,
                    },
                }
            }
            state
                .journals()
                .get(&journal.operation_id)
                .and_then(|entry| performance_terminal_intent(&entry))
                .ok_or(OperationJournalStoreError::AlreadyExists)?
        }
    };
    let succeeded = intent.succeeded;
    let result_target_id = intent.result_target_id;
    let expected = PerformanceJournalTransition::terminal(
        action,
        &base_target_id,
        &result_target_id,
        journal.rollback,
        succeeded,
    );
    loop {
        let result = if succeeded {
            record_performance_operation_success(
                state,
                &journal.operation_id,
                action,
                &result_target_id,
                journal.rollback,
            )
            .await
        } else {
            record_performance_operation_failure(
                state,
                &journal.operation_id,
                action,
                &result_target_id,
                journal.rollback,
            )
            .await
        };
        match result {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if state
                    .journals()
                    .get(&journal.operation_id)
                    .as_ref()
                    .is_some_and(|entry| expected.matches(entry)) =>
            {
                return Ok(());
            }
            Err(error) => {
                let _ = state
                    .journals()
                    .reconcile_transition(
                        &journal.operation_id,
                        error,
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_INITIAL_DELAY_MS),
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_MAX_DELAY_MS),
                        |entry| expected.matches(entry),
                    )
                    .await?;
                if state
                    .journals()
                    .get(&journal.operation_id)
                    .is_some_and(|entry| expected.matches(&entry))
                {
                    return Ok(());
                }
            }
        }
    }
}

async fn terminalize_invalid_performance_journal(
    state: &AppState,
    journal: &OperationJournalEntry,
) -> Result<(), OperationJournalStoreError> {
    if journal.command != CommandKind::ApplyPerformancePlan {
        return Err(OperationJournalStoreError::AlreadyExists);
    }
    let mut failure_step = OperationJournalStep::new(
        PERFORMANCE_INVALID_JOURNAL_FAILURE_POINT,
        OperationPhase::Failed,
    );
    failure_step.result = OperationStepResult::Failed;
    failure_step.rollback = journal.rollback;
    let mut expected = journal.clone();
    expected.status = OperationStatus::Failed;
    expected.completed_steps.push(failure_step.clone());
    expected.failure_point = Some(PERFORMANCE_INVALID_JOURNAL_FAILURE_POINT.to_string());
    expected.outcome = Some(OperationOutcome::Failed);

    loop {
        match state
            .journals()
            .record_failure(
                &journal.operation_id,
                failure_step.clone(),
                PERFORMANCE_INVALID_JOURNAL_FAILURE_POINT,
                OperationOutcome::Failed,
            )
            .await
        {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyTerminal)
                if state
                    .journals()
                    .get(&journal.operation_id)
                    .is_some_and(|entry| entry == expected) =>
            {
                return Ok(());
            }
            Err(error) => {
                let _ = state
                    .journals()
                    .reconcile_transition(
                        &journal.operation_id,
                        error,
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_INITIAL_DELAY_MS),
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_MAX_DELAY_MS),
                        |entry| entry == &expected,
                    )
                    .await?;
                if state
                    .journals()
                    .get(&journal.operation_id)
                    .is_some_and(|entry| entry == expected)
                {
                    return Ok(());
                }
            }
        }
    }
}

pub(super) async fn run_queued_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: String,
    foreground: IntegrityForegroundLease,
) {
    run_owned_performance_operation(state, operation, store, install_id, None, &foreground).await;
}

async fn run_owned_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: String,
    mut completion: Option<
        tokio::sync::oneshot::Sender<
            Result<PerformanceInstallResponse, PerformanceApplicationError>,
        >,
    >,
    foreground: &IntegrityForegroundLease,
) {
    record_performance_progress_status(&state, &install_id, "queued").await;
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
    record_performance_progress_status(&state, &install_id, "planning").await;
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
    record_performance_progress_status(
        &state,
        &install_id,
        operation_progress_phase(operation.action),
    )
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

    loop {
        let mut reapply_requested_mutation = false;
        match execute_performance_operation(&state, &operation, foreground).await {
            Ok(response) => {
                let operation_id = OperationId::new(install_id.clone());
                let Some(journal) = state.journals().get(&operation_id) else {
                    terminalize_uncertain_performance_operation(
                        &state,
                        &store,
                        &install_id,
                        Some(operation.action),
                        PERFORMANCE_RECONCILIATION_FAILURE,
                        operation.persistence_failure.as_ref(),
                    )
                    .await;
                    send_performance_completion_error(&mut completion);
                    return;
                };
                if publish_performance_terminal(
                    &state,
                    &store,
                    &install_id,
                    &journal,
                    Some(complete_progress_label(&response.status)),
                    PERFORMANCE_RECONCILIATION_FAILURE,
                    operation.persistence_failure.as_ref(),
                )
                .await
                    && let Some(completion) = completion.take()
                {
                    let _ = completion.send(Ok(response));
                }
                return;
            }
            Err(PerformanceOperationExecutionError::Operation(error)) => {
                let published = terminalize_uncertain_performance_operation(
                    &state,
                    &store,
                    &install_id,
                    Some(operation.action),
                    &error_message(&error),
                    operation.persistence_failure.as_ref(),
                )
                .await;
                if published && let Some(completion) = completion.take() {
                    let _ = completion.send(Err(error));
                } else {
                    send_performance_completion_error(&mut completion);
                }
                return;
            }
            Err(PerformanceOperationExecutionError::Journal {
                error,
                operation_id,
                expected,
            }) => {
                if matches!(&error, OperationJournalStoreError::Persistence(_)) {
                    if let Some(signal) = &operation.persistence_failure {
                        signal.notify();
                    }
                    send_performance_completion_error(&mut completion);
                }
                let Some((operation_id, expected)) = operation_id.zip(expected) else {
                    tracing::warn!(
                        operation_id = install_id.as_str(),
                        journal_error = error.class(),
                        "performance operation journal failure lacked a transition identity"
                    );
                    terminalize_uncertain_performance_operation(
                        &state,
                        &store,
                        &install_id,
                        Some(operation.action),
                        PERFORMANCE_RECONCILIATION_FAILURE,
                        operation.persistence_failure.as_ref(),
                    )
                    .await;
                    return;
                };
                match reconcile_performance_journal_transition(
                    &state,
                    &operation_id,
                    error,
                    &expected,
                )
                .await
                {
                    Ok(JournalRetryOutcome::RequestedTransitionCommitted) => {
                        reapply_requested_mutation = operation.persistence_failure.is_none();
                    }
                    Ok(JournalRetryOutcome::RetryRequestedTransition) => {
                        continue;
                    }
                    Err(error) => {
                        if let Some(signal) = &operation.persistence_failure {
                            signal.notify();
                        }
                        send_performance_completion_error(&mut completion);
                        tracing::warn!(
                            operation_id = install_id.as_str(),
                            journal_error = error.class(),
                            "performance operation journal reconciliation was rejected"
                        );
                        terminalize_uncertain_performance_operation(
                            &state,
                            &store,
                            &install_id,
                            Some(operation.action),
                            PERFORMANCE_RECONCILIATION_FAILURE,
                            operation.persistence_failure.as_ref(),
                        )
                        .await;
                        return;
                    }
                }
            }
            Err(PerformanceOperationExecutionError::Status(error)) => {
                if let Some(signal) = &operation.persistence_failure {
                    signal.notify();
                }
                send_performance_completion_error(&mut completion);
                tracing::warn!(
                    operation_id = install_id.as_str(),
                    status_error = error.class(),
                    "performance operation status transition requires journal reconciliation"
                );
                let operation_id = OperationId::new(install_id.clone());
                if let Err(error) = retry_performance_status_transition(
                    &state,
                    &operation_id,
                    PERFORMANCE_EFFECT_STARTED_STATE,
                    None,
                    Err(error),
                    operation.persistence_failure.as_ref(),
                )
                .await
                {
                    tracing::warn!(
                        operation_id = install_id.as_str(),
                        status_error = error.class(),
                        "performance effect-started status reconciliation was rejected"
                    );
                }
            }
        }

        if reconcile_performance_journal_after_execution(
            &state,
            &store,
            &install_id,
            Some(operation.action),
        )
        .await
        {
            if reapply_requested_mutation {
                continue;
            }
            terminalize_uncertain_performance_operation(
                &state,
                &store,
                &install_id,
                Some(operation.action),
                PERFORMANCE_RECONCILIATION_FAILURE,
                operation.persistence_failure.as_ref(),
            )
            .await;
        }
        return;
    }
}

fn send_performance_completion_error(
    completion: &mut Option<
        tokio::sync::oneshot::Sender<
            Result<PerformanceInstallResponse, PerformanceApplicationError>,
        >,
    >,
) {
    if let Some(completion) = completion.take() {
        let _ = completion.send(Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
        )));
    }
}

async fn prepare_resumed_performance_operation(
    state: &AppState,
    status: &PerformanceOperationStatus,
    store: &crate::state::InstallStore,
) -> Option<PerformanceOperation> {
    let operation_id = OperationId::new(status.id.clone());
    if matches!(
        status.state.as_str(),
        crate::state::performance_operations::PERFORMANCE_RESUME_BLOCKED_STATE
            | crate::state::performance_operations::PERFORMANCE_RESUME_INVALID_STATE
    ) {
        terminalize_mismatched_performance_operation(
            state,
            store,
            status,
            operation_action_from_name(&status.action).unwrap_or(PerformanceInstallAction::Install),
            INVALID_PERSISTED_OPERATION_ERROR,
        )
        .await;
        return None;
    }

    match operation_from_status(status) {
        Ok(operation) => {
            let Some(identity) = status.journal_identity.as_ref() else {
                terminalize_uncertain_performance_operation(
                    state,
                    store,
                    &status.id,
                    Some(operation.action),
                    PERFORMANCE_RECONCILIATION_FAILURE,
                    None,
                )
                .await;
                return None;
            };
            let effective_action = operation_action_from_name(&identity.action)
                .unwrap_or(PerformanceInstallAction::Install);
            if let Some(journal) = state.journals().get(&operation_id) {
                if !performance_journal_matches_status(&journal, status) {
                    terminalize_mismatched_performance_operation(
                        state,
                        store,
                        status,
                        effective_action,
                        PERFORMANCE_RECONCILIATION_FAILURE,
                    )
                    .await;
                    return None;
                }
                if performance_journal_is_terminal(journal.status) {
                    if performance_terminal_transition(&journal).is_none() {
                        terminalize_mismatched_performance_operation(
                            state,
                            store,
                            status,
                            effective_action,
                            PERFORMANCE_RECONCILIATION_FAILURE,
                        )
                        .await;
                        return None;
                    }
                    publish_performance_terminal(
                        state,
                        store,
                        &status.id,
                        &journal,
                        None,
                        PERFORMANCE_RECONCILIATION_FAILURE,
                        None,
                    )
                    .await;
                    return None;
                }
                if performance_terminal_intent(&journal).is_some() {
                    finish_performance_terminal_intent(state, store, status, &journal).await;
                    return None;
                }
                if performance_journal_has_terminal_marker(&journal) {
                    terminalize_mismatched_performance_operation(
                        state,
                        store,
                        status,
                        effective_action,
                        PERFORMANCE_RECONCILIATION_FAILURE,
                    )
                    .await;
                    return None;
                }
                if performance_journal_has_fact(&journal, PERFORMANCE_EFFECT_STARTED_FACT) {
                    terminalize_uncertain_performance_operation(
                        state,
                        store,
                        &status.id,
                        Some(effective_action),
                        PERFORMANCE_RECONCILIATION_FAILURE,
                        None,
                    )
                    .await;
                    return None;
                }
                if performance_status_requires_reconciliation(&status.state) {
                    terminalize_uncertain_performance_operation(
                        state,
                        store,
                        &status.id,
                        Some(effective_action),
                        PERFORMANCE_RECONCILIATION_FAILURE,
                        None,
                    )
                    .await;
                    return None;
                }
            } else if performance_status_requires_reconciliation(&status.state) {
                terminalize_uncertain_performance_operation(
                    state,
                    store,
                    &status.id,
                    Some(effective_action),
                    PERFORMANCE_RECONCILIATION_FAILURE,
                    None,
                )
                .await;
                return None;
            }
            Some(operation)
        }
        Err(error) => {
            terminalize_uncertain_performance_operation(
                state,
                store,
                &status.id,
                operation_action_from_name(&status.action),
                &error,
                None,
            )
            .await;
            None
        }
    }
}

fn performance_status_requires_reconciliation(status: &str) -> bool {
    matches!(
        status,
        PERFORMANCE_EFFECT_STARTED_STATE
            | PERFORMANCE_COMMITTING_COMPLETE_STATE
            | PERFORMANCE_COMMITTING_FAILED_STATE
    )
}

async fn reconcile_performance_journal_after_execution(
    state: &AppState,
    store: &crate::state::InstallStore,
    install_id: &str,
    action: Option<PerformanceInstallAction>,
) -> bool {
    let operation_id = OperationId::new(install_id.to_string());
    let Some(journal) = state.journals().get(&operation_id) else {
        terminalize_uncertain_performance_operation(
            state,
            store,
            install_id,
            action,
            PERFORMANCE_RECONCILIATION_FAILURE,
            None,
        )
        .await;
        return false;
    };
    if performance_journal_is_terminal(journal.status) {
        if performance_terminal_transition(&journal).is_none() {
            let Some(status) = state.performance_operations().get(install_id).await else {
                return false;
            };
            terminalize_mismatched_performance_operation(
                state,
                store,
                &status,
                action.unwrap_or(PerformanceInstallAction::Install),
                PERFORMANCE_RECONCILIATION_FAILURE,
            )
            .await;
            return false;
        }
        publish_performance_terminal(
            state,
            store,
            install_id,
            &journal,
            None,
            PERFORMANCE_RECONCILIATION_FAILURE,
            None,
        )
        .await;
        return false;
    }
    if performance_terminal_intent(&journal).is_some() {
        let Some(status) = state.performance_operations().get(install_id).await else {
            return false;
        };
        finish_performance_terminal_intent(state, store, &status, &journal).await;
        return false;
    }
    if performance_journal_has_fact(&journal, PERFORMANCE_EFFECT_STARTED_FACT) {
        terminalize_uncertain_performance_operation(
            state,
            store,
            install_id,
            action,
            PERFORMANCE_RECONCILIATION_FAILURE,
            None,
        )
        .await;
        return false;
    }
    if journal.command == CommandKind::ApplyPerformancePlan
        && performance_journal_has_fact(&journal, PERFORMANCE_EFFECT_GATE_FACT)
    {
        return true;
    }

    terminalize_uncertain_performance_operation(
        state,
        store,
        install_id,
        action,
        PERFORMANCE_RECONCILIATION_FAILURE,
        None,
    )
    .await;
    false
}

async fn finish_performance_terminal_intent(
    state: &AppState,
    store: &crate::state::InstallStore,
    status: &PerformanceOperationStatus,
    journal: &OperationJournalEntry,
) {
    let Some(intent) = performance_terminal_intent(journal) else {
        return;
    };
    let succeeded = intent.succeeded;
    let action = intent.action;
    let base_target_id = intent.base_target_id;
    let result_target_id = intent.result_target_id;
    loop {
        let result = if succeeded {
            record_performance_operation_success(
                state,
                &journal.operation_id,
                action,
                &result_target_id,
                intent.rollback,
            )
            .await
        } else {
            record_performance_operation_failure(
                state,
                &journal.operation_id,
                action,
                &result_target_id,
                intent.rollback,
            )
            .await
        };
        let expected = PerformanceJournalTransition::terminal(
            action,
            &base_target_id,
            &result_target_id,
            intent.rollback,
            succeeded,
        );
        match result {
            Ok(()) => break,
            Err(OperationJournalStoreError::AlreadyTerminal)
                if state
                    .journals()
                    .get(&journal.operation_id)
                    .as_ref()
                    .is_some_and(|entry| expected.matches(entry)) =>
            {
                break;
            }
            Err(error) => match reconcile_performance_journal_transition(
                state,
                &journal.operation_id,
                error,
                &expected,
            )
            .await
            {
                Ok(JournalRetryOutcome::RequestedTransitionCommitted) => break,
                Ok(JournalRetryOutcome::RetryRequestedTransition) => continue,
                Err(error) => {
                    tracing::warn!(
                        operation_id = status.id.as_str(),
                        journal_error = error.class(),
                        "performance terminal intent reconciliation was rejected"
                    );
                    return;
                }
            },
        }
    }
    if let Some(terminal) = state.journals().get(&journal.operation_id) {
        publish_performance_terminal(
            state,
            store,
            &status.id,
            &terminal,
            None,
            status
                .error
                .as_deref()
                .unwrap_or(PERFORMANCE_RECONCILIATION_FAILURE),
            None,
        )
        .await;
    }
}

async fn terminalize_uncertain_performance_operation(
    state: &AppState,
    store: &crate::state::InstallStore,
    install_id: &str,
    action: Option<PerformanceInstallAction>,
    error_message: &str,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> bool {
    let operation_id = OperationId::new(install_id.to_string());
    let action = action.unwrap_or(PerformanceInstallAction::Install);
    let Some(status) = state.performance_operations().get(install_id).await else {
        return false;
    };
    let (reconciliation_action, reconciliation_target_id, reconciliation_rollback) = status
        .journal_identity
        .as_ref()
        .and_then(|identity| {
            operation_action_from_name(&identity.action)
                .map(|action| (action, identity.target_id.clone(), identity.rollback))
        })
        .unwrap_or_else(|| {
            (
                action,
                "performance_reconciliation".to_string(),
                RollbackState::Unavailable,
            )
        });
    while state.journals().get(&operation_id).is_none() {
        match begin_performance_reconciliation_journal(state, action, install_id).await {
            Ok(()) => {}
            Err(error) => match reconcile_performance_journal_transition(
                state,
                &operation_id,
                error,
                &PerformanceJournalTransition::created(
                    reconciliation_action,
                    &reconciliation_target_id,
                    reconciliation_rollback,
                ),
            )
            .await
            {
                Ok(JournalRetryOutcome::RequestedTransitionCommitted) => {}
                Ok(JournalRetryOutcome::RetryRequestedTransition) => continue,
                Err(error) => {
                    tracing::warn!(
                        operation_id = install_id,
                        journal_error = error.class(),
                        "performance failure journal creation was rejected"
                    );
                    return false;
                }
            },
        }
    }
    let Some(mut journal) = state.journals().get(&operation_id) else {
        return false;
    };
    if !performance_journal_matches_status(&journal, &status) {
        return terminalize_mismatched_performance_operation(
            state,
            store,
            &status,
            action,
            error_message,
        )
        .await;
    }
    if performance_journal_is_terminal(journal.status) {
        if performance_terminal_transition(&journal).is_none() {
            return terminalize_mismatched_performance_operation(
                state,
                store,
                &status,
                action,
                error_message,
            )
            .await;
        }
        return publish_performance_terminal(
            state,
            store,
            install_id,
            &journal,
            None,
            error_message,
            failure_signal,
        )
        .await;
    }

    let Some(identity) = performance_journal_identity(&journal) else {
        return false;
    };
    let journal_action = identity.action;
    let base_target_id = identity.target_id;
    let result_target_id = performance_terminal_intent(&journal)
        .map(|intent| intent.result_target_id)
        .unwrap_or_else(|| base_target_id.clone());
    if performance_terminal_intent(&journal).is_none() {
        loop {
            match record_performance_terminal_intent(
                state,
                &operation_id,
                journal_action,
                &result_target_id,
                journal.rollback,
                false,
            )
            .await
            {
                Ok(()) => break,
                Err(error) => match reconcile_performance_journal_transition(
                    state,
                    &operation_id,
                    error,
                    &PerformanceJournalTransition::terminal_intent(
                        journal_action,
                        &base_target_id,
                        &result_target_id,
                        journal.rollback,
                        false,
                    ),
                )
                .await
                {
                    Ok(JournalRetryOutcome::RequestedTransitionCommitted) => break,
                    Ok(JournalRetryOutcome::RetryRequestedTransition) => continue,
                    Err(error) => {
                        tracing::warn!(
                            operation_id = install_id,
                            journal_error = error.class(),
                            "performance failure intent reconciliation was rejected"
                        );
                        return false;
                    }
                },
            }
        }
        let Some(updated) = state.journals().get(&operation_id) else {
            return false;
        };
        journal = updated;
    }
    if !performance_journal_is_terminal(journal.status) {
        loop {
            let expected = PerformanceJournalTransition::terminal(
                journal_action,
                &base_target_id,
                &result_target_id,
                journal.rollback,
                false,
            );
            match record_performance_operation_failure(
                state,
                &operation_id,
                journal_action,
                &result_target_id,
                journal.rollback,
            )
            .await
            {
                Ok(()) => break,
                Err(OperationJournalStoreError::AlreadyTerminal) => {
                    let Some(terminal) = state.journals().get(&operation_id) else {
                        return false;
                    };
                    if !performance_journal_is_terminal(terminal.status)
                        || !performance_journal_matches_status(&terminal, &status)
                    {
                        return false;
                    }
                    break;
                }
                Err(error) => match reconcile_performance_journal_transition(
                    state,
                    &operation_id,
                    error,
                    &expected,
                )
                .await
                {
                    Ok(JournalRetryOutcome::RequestedTransitionCommitted) => break,
                    Ok(JournalRetryOutcome::RetryRequestedTransition) => continue,
                    Err(error) => {
                        tracing::warn!(
                            operation_id = install_id,
                            journal_error = error.class(),
                            "performance failure terminal reconciliation was rejected"
                        );
                        return false;
                    }
                },
            }
        }
    }
    if let Some(terminal) = state.journals().get(&operation_id) {
        return publish_performance_terminal(
            state,
            store,
            install_id,
            &terminal,
            None,
            error_message,
            failure_signal,
        )
        .await;
    }
    false
}

pub(super) async fn terminalize_mismatched_performance_operation(
    state: &AppState,
    store: &crate::state::InstallStore,
    status: &PerformanceOperationStatus,
    action: PerformanceInstallAction,
    error_message: &str,
) -> bool {
    let operation_id = OperationId::new(status.id.clone());
    if let Some(journal) = state.journals().get(&operation_id)
        && journal.command == CommandKind::ApplyPerformancePlan
        && !performance_journal_is_terminal(journal.status)
        && let Err(error) = terminalize_orphaned_performance_journal(state, &journal).await
    {
        tracing::warn!(
            operation_id = status.id.as_str(),
            journal_error = error.class(),
            "mismatched performance journal terminalization was rejected"
        );
        return false;
    }

    let reconciliation_id = OperationId::new(format!("{}-reconciliation", status.id));
    if let Err(error) =
        commit_mismatched_performance_reconciliation(state, &reconciliation_id, action).await
    {
        tracing::warn!(
            operation_id = status.id.as_str(),
            journal_error = error.class(),
            "performance mismatch reconciliation journal was rejected"
        );
        return false;
    }

    let message = sanitize_operation_error(error_message);
    let result = state
        .performance_operations()
        .record_reconciliation_failed(&status.id, &message, operation_action_name(action))
        .await;
    let status_result =
        retry_performance_status_correction(state, &operation_id, action, &message, result).await;
    if status_result.is_err() {
        return false;
    }
    emit_performance_progress(store, &status.id, "error", 4, 4, None, Some(message), true).await;
    true
}

async fn commit_mismatched_performance_reconciliation(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
) -> Result<(), OperationJournalStoreError> {
    let expected = mismatched_reconciliation_entry(operation_id, action);
    loop {
        match state.journals().create(expected.clone()).await {
            Ok(()) => return Ok(()),
            Err(OperationJournalStoreError::AlreadyExists)
                if mismatched_reconciliation_committed(state, operation_id, action) =>
            {
                return Ok(());
            }
            Err(error) => {
                let _ = state
                    .journals()
                    .reconcile_transition(
                        operation_id,
                        error,
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_INITIAL_DELAY_MS),
                        std::time::Duration::from_millis(PERFORMANCE_RETRY_MAX_DELAY_MS),
                        |entry| entry == &expected,
                    )
                    .await?;
                if mismatched_reconciliation_committed(state, operation_id, action) {
                    return Ok(());
                }
            }
        }
    }
}

fn mismatched_reconciliation_entry(
    operation_id: &OperationId,
    action: PerformanceInstallAction,
) -> OperationJournalEntry {
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::ApplyPerformancePlan,
        StabilizationSystem::Application,
        StateOwnershipClass::CompositionManaged,
        RollbackState::Unavailable,
    );
    entry.status = OperationStatus::Failed;
    entry.targets = performance_operation_targets("performance_reconciliation");
    entry.planned_steps.push(performance_operation_journal_step(
        action,
        OperationStepResult::Planned,
        "performance_reconciliation",
        RollbackState::Unavailable,
    ));
    entry
        .completed_steps
        .push(performance_operation_journal_step(
            action,
            OperationStepResult::Failed,
            "performance_reconciliation",
            RollbackState::Unavailable,
        ));
    entry.failure_point = Some("performance_journal_identity_mismatch".to_string());
    entry.outcome = Some(OperationOutcome::Failed);
    entry
}

fn mismatched_reconciliation_committed(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
) -> bool {
    state
        .journals()
        .get(operation_id)
        .is_some_and(|entry| entry == mismatched_reconciliation_entry(operation_id, action))
}

fn performance_status_has_mismatch_reconciliation(
    state: &AppState,
    status: &PerformanceOperationStatus,
) -> bool {
    status.state == "failed"
        && status.error.is_some()
        && status.journal_identity.as_ref().is_some_and(|identity| {
            identity.action == status.action
                && identity.target_id == "performance_reconciliation"
                && identity.rollback == RollbackState::Unavailable
        })
        && status
            .journal_identity
            .as_ref()
            .and_then(|identity| operation_action_from_name(&identity.action))
            .is_some_and(|action| {
                mismatched_reconciliation_committed(
                    state,
                    &OperationId::new(format!("{}-reconciliation", status.id)),
                    action,
                )
            })
}

async fn publish_performance_terminal(
    state: &AppState,
    store: &crate::state::InstallStore,
    install_id: &str,
    journal: &OperationJournalEntry,
    complete_label: Option<&str>,
    fallback_error: &str,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> bool {
    let Some(status) = state.performance_operations().get(install_id).await else {
        return false;
    };
    if !performance_journal_matches_status(journal, &status)
        || performance_terminal_transition(journal).is_none()
    {
        tracing::warn!(
            operation_id = install_id,
            "performance terminal journal did not match durable status identity"
        );
        return false;
    }
    let operation_id = OperationId::new(install_id.to_string());
    match journal.status {
        OperationStatus::Succeeded => {
            let result = state
                .performance_operations()
                .record_complete(install_id)
                .await;
            if let Err(error) = retry_performance_status_transition(
                state,
                &operation_id,
                "complete",
                None,
                result,
                failure_signal,
            )
            .await
            {
                tracing::warn!(
                    operation_id = install_id,
                    status_error = error.class(),
                    "performance success status publication was rejected"
                );
                return false;
            }
            let action = performance_journal_action(journal)
                .or_else(|| operation_action_from_name(&status.action))
                .unwrap_or(PerformanceInstallAction::Install);
            emit_performance_progress(
                store,
                install_id,
                "complete",
                4,
                4,
                Some(complete_label.unwrap_or_else(|| complete_progress_label_for_action(action))),
                None,
                true,
            )
            .await;
            true
        }
        OperationStatus::Failed | OperationStatus::Blocked | OperationStatus::Cancelled => {
            let message = status
                .error
                .as_deref()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or(fallback_error);
            let message = sanitize_operation_error(message);
            let result = state
                .performance_operations()
                .record_failed(install_id, &message)
                .await;
            if let Err(error) = retry_performance_status_transition(
                state,
                &operation_id,
                "failed",
                Some(&message),
                result,
                failure_signal,
            )
            .await
            {
                tracing::warn!(
                    operation_id = install_id,
                    status_error = error.class(),
                    "performance failure status publication was rejected"
                );
                return false;
            }
            emit_performance_progress(store, install_id, "error", 4, 4, None, Some(message), true)
                .await;
            true
        }
        OperationStatus::Requested
        | OperationStatus::Planned
        | OperationStatus::Running
        | OperationStatus::WaitingForUser => false,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum JournalRetryOutcome {
    RequestedTransitionCommitted,
    RetryRequestedTransition,
}

async fn reconcile_performance_journal_transition(
    state: &AppState,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: &PerformanceJournalTransition,
) -> Result<JournalRetryOutcome, OperationJournalStoreError> {
    let _ = state
        .journals()
        .reconcile_transition(
            operation_id,
            error,
            std::time::Duration::from_millis(PERFORMANCE_RETRY_INITIAL_DELAY_MS),
            std::time::Duration::from_millis(PERFORMANCE_RETRY_MAX_DELAY_MS),
            |entry| expected.matches(entry),
        )
        .await?;
    Ok(
        if state
            .journals()
            .get(operation_id)
            .as_ref()
            .is_some_and(|entry| expected.matches(entry))
        {
            JournalRetryOutcome::RequestedTransitionCommitted
        } else {
            JournalRetryOutcome::RetryRequestedTransition
        },
    )
}

async fn record_performance_progress_status(state: &AppState, operation_id: &str, phase: &str) {
    if let Err(error) = state
        .performance_operations()
        .record_progress(operation_id, phase)
        .await
    {
        tracing::warn!(
            operation_id,
            status_error = error.class(),
            "performance operation progress status was not accepted"
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedPerformanceJournalIdentity {
    action: PerformanceInstallAction,
    target_id: String,
    rollback: RollbackState,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ParsedPerformanceTerminalIntent {
    action: PerformanceInstallAction,
    base_target_id: String,
    result_target_id: String,
    rollback: RollbackState,
    succeeded: bool,
}

fn performance_terminal_intent(
    journal: &OperationJournalEntry,
) -> Option<ParsedPerformanceTerminalIntent> {
    let identity = performance_journal_identity(journal)?;
    let mut marked_steps = journal.completed_steps.iter().filter(|step| {
        step.generated_facts.iter().any(|fact| {
            matches!(
                fact.as_str(),
                PERFORMANCE_TERMINAL_SUCCESS_FACT | PERFORMANCE_TERMINAL_FAILURE_FACT
            )
        })
    });
    let step = marked_steps.next()?;
    if marked_steps.next().is_some() {
        return None;
    }
    let success_markers = step
        .generated_facts
        .iter()
        .filter(|fact| fact.as_str() == PERFORMANCE_TERMINAL_SUCCESS_FACT)
        .count();
    let failure_markers = step
        .generated_facts
        .iter()
        .filter(|fact| fact.as_str() == PERFORMANCE_TERMINAL_FAILURE_FACT)
        .count();
    if success_markers + failure_markers != 1 {
        return None;
    }
    let succeeded = success_markers == 1;
    let result_target_id = step.changed_target.as_ref()?.id.clone();
    let mut expected = performance_operation_journal_step(
        identity.action,
        OperationStepResult::Completed,
        &result_target_id,
        identity.rollback,
    );
    expected.step_id = "performance_terminal_intent".to_string();
    expected.generated_facts.push(
        if succeeded {
            PERFORMANCE_TERMINAL_SUCCESS_FACT
        } else {
            PERFORMANCE_TERMINAL_FAILURE_FACT
        }
        .to_string(),
    );
    if step != &expected {
        return None;
    }

    Some(ParsedPerformanceTerminalIntent {
        action: identity.action,
        base_target_id: identity.target_id,
        result_target_id,
        rollback: identity.rollback,
        succeeded,
    })
}

fn performance_terminal_transition(
    journal: &OperationJournalEntry,
) -> Option<ParsedPerformanceTerminalIntent> {
    let intent = performance_terminal_intent(journal)?;
    let expected_status = if intent.succeeded {
        OperationStatus::Succeeded
    } else {
        OperationStatus::Failed
    };
    let expected_outcome = if intent.succeeded {
        OperationOutcome::Succeeded
    } else {
        OperationOutcome::Failed
    };
    let expected_failure_point =
        (!intent.succeeded).then(|| performance_operation_step_id(intent.action).to_string());
    if journal.status != expected_status
        || journal.outcome != Some(expected_outcome)
        || journal.failure_point != expected_failure_point
    {
        return None;
    }
    let mut terminal_steps = journal.completed_steps.iter().filter(|step| {
        matches!(
            step.step_id.as_str(),
            "apply_performance_plan" | "remove_performance_plan" | "rollback_performance_plan"
        )
    });
    let terminal_step = terminal_steps.next()?;
    if terminal_steps.next().is_some() {
        return None;
    }
    let terminal_rollback =
        if intent.succeeded && matches!(intent.action, PerformanceInstallAction::Rollback) {
            RollbackState::Applied
        } else {
            intent.rollback
        };
    let expected_step = performance_operation_journal_step(
        intent.action,
        if intent.succeeded {
            OperationStepResult::Completed
        } else {
            OperationStepResult::Failed
        },
        &intent.result_target_id,
        terminal_rollback,
    );
    (terminal_step == &expected_step).then_some(intent)
}

fn performance_journal_has_fact(journal: &OperationJournalEntry, expected: &str) -> bool {
    journal
        .planned_steps
        .iter()
        .chain(journal.completed_steps.iter())
        .any(|step| step.generated_facts.iter().any(|fact| fact == expected))
}

fn performance_journal_has_terminal_marker(journal: &OperationJournalEntry) -> bool {
    journal.completed_steps.iter().any(|step| {
        step.generated_facts.iter().any(|fact| {
            matches!(
                fact.as_str(),
                PERFORMANCE_TERMINAL_SUCCESS_FACT | PERFORMANCE_TERMINAL_FAILURE_FACT
            )
        })
    })
}

fn performance_journal_action(journal: &OperationJournalEntry) -> Option<PerformanceInstallAction> {
    performance_journal_identity(journal).map(|identity| identity.action)
}

fn performance_journal_identity(
    journal: &OperationJournalEntry,
) -> Option<ParsedPerformanceJournalIdentity> {
    if journal.command != CommandKind::ApplyPerformancePlan
        || journal.owner != StabilizationSystem::Application
        || journal.ownership != StateOwnershipClass::CompositionManaged
    {
        return None;
    }

    let [step] = journal.planned_steps.as_slice() else {
        return None;
    };
    let action = match step.step_id.as_str() {
        "apply_performance_plan" => PerformanceInstallAction::Install,
        "remove_performance_plan" => PerformanceInstallAction::Remove,
        "rollback_performance_plan" => PerformanceInstallAction::Rollback,
        _ => return None,
    };
    let target_id = journal.targets.first()?.id.clone();
    if journal.targets != performance_operation_targets(&target_id) {
        return None;
    }
    let mut expected = performance_operation_journal_step(
        action,
        OperationStepResult::Planned,
        &target_id,
        journal.rollback,
    );
    expected
        .generated_facts
        .push(PERFORMANCE_EFFECT_GATE_FACT.to_string());
    if step != &expected {
        return None;
    }

    Some(ParsedPerformanceJournalIdentity {
        action,
        target_id,
        rollback: journal.rollback,
    })
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

async fn durable_performance_operation_identity(
    state: &AppState,
    operation: &PerformanceOperation,
    foreground: &IntegrityForegroundLease,
) -> PerformanceOperationJournalIdentity {
    performance_operation_journal_identity(state, operation, foreground)
        .await
        .map(|identity| {
            PerformanceOperationJournalIdentity::new(
                operation_action_name(identity.action),
                identity.target_id,
                identity.rollback,
            )
        })
        .unwrap_or_else(|_| {
            PerformanceOperationJournalIdentity::new(
                operation_action_name(operation.action),
                "performance_reconciliation",
                RollbackState::Unavailable,
            )
        })
}

fn public_performance_operation_status(
    state: &AppState,
    mut status: PerformanceOperationStatus,
) -> PerformanceOperationStatusResponse {
    let proof = performance_operation_proof(state, &status);
    status.instance_id = public_operation_required_token(&status.instance_id, "redacted");
    status.action = public_operation_required_token(&status.action, "unknown");
    status.state = public_operation_required_token(&status.state, "unknown");
    status.created_at = public_operation_timestamp(&status.created_at);
    status.updated_at = public_operation_timestamp(&status.updated_at);
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
        .filter(|journal| performance_terminal_matches_status(journal, status))
        .map(|journal| operation_journal_proof_record(&journal))
}

fn performance_status_is_terminal(status: &str) -> bool {
    matches!(status, "complete" | "failed" | "interrupted")
}

pub(super) fn performance_journal_is_terminal(status: OperationStatus) -> bool {
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

fn public_operation_timestamp(value: &str) -> String {
    normalized_operation_timestamp(value).unwrap_or_else(|| "unknown".to_string())
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
        persistence_failure: None,
        installed_versions: None,
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

fn complete_progress_label_for_action(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "Managed performance bundle updated",
        PerformanceInstallAction::Remove => "Managed performance files removed",
        PerformanceInstallAction::Rollback => "Managed performance files rolled back",
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

pub(super) async fn begin_performance_operation_journal(
    state: &AppState,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
    linked_operation_id: Option<&str>,
) -> Result<OperationId, OperationJournalStoreError> {
    let operation_id = linked_operation_id
        .map(OperationId::new)
        .unwrap_or_else(|| generated_performance_journal_operation_id(action));
    if linked_operation_id.is_some() {
        let Some(status) = state
            .performance_operations()
            .get(operation_id.as_str())
            .await
        else {
            return Err(OperationJournalStoreError::MissingOperation);
        };
        if !status_identity_matches_requested(&status, action, target_id, rollback) {
            return Err(OperationJournalStoreError::AlreadyExists);
        }
    }
    if linked_operation_id.is_some()
        && let Some(existing) = state.journals().get(&operation_id)
    {
        if performance_journal_is_terminal(existing.status) {
            return Err(OperationJournalStoreError::AlreadyTerminal);
        }
        if performance_journal_matches(&existing, action, target_id, rollback) {
            return Ok(operation_id);
        }
        return Err(OperationJournalStoreError::AlreadyExists);
    }
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
    if let Some(step) = entry.planned_steps.last_mut() {
        step.generated_facts
            .push(PERFORMANCE_EFFECT_GATE_FACT.to_string());
    }
    state.journals().create(entry).await?;
    Ok(operation_id)
}

fn status_identity_matches_requested(
    status: &PerformanceOperationStatus,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> bool {
    status.journal_identity.as_ref().is_some_and(|identity| {
        identity.action == operation_action_name(action)
            && identity.target_id == target_id
            && identity.rollback == rollback
    })
}

fn performance_journal_matches_status(
    journal: &OperationJournalEntry,
    status: &PerformanceOperationStatus,
) -> bool {
    journal.operation_id.as_str() == status.id
        && status.journal_identity.as_ref().is_some_and(|identity| {
            let Some(action) = operation_action_from_name(&identity.action) else {
                return false;
            };
            performance_journal_matches(journal, action, &identity.target_id, identity.rollback)
        })
}

fn performance_terminal_matches_status(
    journal: &OperationJournalEntry,
    status: &PerformanceOperationStatus,
) -> bool {
    performance_journal_matches_status(journal, status)
        && performance_terminal_transition(journal).is_some_and(|intent| {
            matches!(
                (status.state.as_str(), intent.succeeded),
                ("complete", true) | ("failed", false)
            )
        })
}

fn performance_journal_matches(
    journal: &OperationJournalEntry,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> bool {
    performance_journal_identity(journal).is_some_and(|identity| {
        identity.action == action
            && identity.target_id == target_id
            && identity.rollback == rollback
    })
}

async fn begin_performance_reconciliation_journal(
    state: &AppState,
    action: PerformanceInstallAction,
    linked_operation_id: &str,
) -> Result<(), OperationJournalStoreError> {
    let operation_id = OperationId::new(linked_operation_id.to_string());
    let identity = state
        .performance_operations()
        .get(linked_operation_id)
        .await
        .and_then(|status| status.journal_identity);
    let action = identity
        .as_ref()
        .and_then(|identity| operation_action_from_name(&identity.action))
        .unwrap_or(action);
    let target_id = identity
        .as_ref()
        .map(|identity| identity.target_id.as_str())
        .unwrap_or("performance_reconciliation");
    let rollback = identity
        .as_ref()
        .map(|identity| identity.rollback)
        .unwrap_or(RollbackState::Unavailable);
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id,
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
    if let Some(step) = entry.planned_steps.last_mut() {
        step.generated_facts
            .push(PERFORMANCE_EFFECT_GATE_FACT.to_string());
    }
    state.journals().create(entry).await
}

pub(super) async fn record_performance_effect_started(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> Result<(), OperationJournalStoreError> {
    let mut step = performance_operation_journal_step(
        action,
        OperationStepResult::Completed,
        target_id,
        rollback,
    );
    step.step_id = "performance_effect_started".to_string();
    step.changed_target = None;
    step.generated_facts
        .push(PERFORMANCE_EFFECT_STARTED_FACT.to_string());
    state.journals().record_checkpoint(operation_id, step).await
}

pub(super) async fn record_performance_terminal_intent(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
    succeeded: bool,
) -> Result<(), OperationJournalStoreError> {
    let mut step = performance_operation_journal_step(
        action,
        OperationStepResult::Completed,
        target_id,
        rollback,
    );
    step.step_id = "performance_terminal_intent".to_string();
    step.generated_facts.push(
        if succeeded {
            PERFORMANCE_TERMINAL_SUCCESS_FACT
        } else {
            PERFORMANCE_TERMINAL_FAILURE_FACT
        }
        .to_string(),
    );
    state.journals().record_checkpoint(operation_id, step).await
}

pub(super) async fn record_performance_effect_started_status(
    state: &AppState,
    operation_id: &OperationId,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> Result<(), PerformanceOperationStoreError> {
    if state
        .performance_operations()
        .get(operation_id.as_str())
        .await
        .is_none()
    {
        return Ok(());
    }
    let result = state
        .performance_operations()
        .record_effect_started(operation_id.as_str())
        .await;
    if result.is_err()
        && let Some(signal) = failure_signal
    {
        signal.notify();
        return result;
    }
    retry_performance_status_transition(
        state,
        operation_id,
        PERFORMANCE_EFFECT_STARTED_STATE,
        None,
        result,
        failure_signal,
    )
    .await
}

async fn record_performance_terminal_intent_status(
    state: &AppState,
    operation_id: &OperationId,
    result: &Result<PerformanceInstallResponse, PerformanceApplicationError>,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> Result<(), PerformanceOperationStoreError> {
    if state
        .performance_operations()
        .get(operation_id.as_str())
        .await
        .is_none()
    {
        return Ok(());
    }
    let (expected_state, expected_error, transition) = match result {
        Ok(_) => (
            PERFORMANCE_COMMITTING_COMPLETE_STATE,
            None,
            state
                .performance_operations()
                .record_committing_complete(operation_id.as_str())
                .await,
        ),
        Err(error) => {
            let message = error_message(error);
            return retry_performance_status_transition(
                state,
                operation_id,
                PERFORMANCE_COMMITTING_FAILED_STATE,
                Some(&message),
                state
                    .performance_operations()
                    .record_committing_failed(operation_id.as_str(), &message)
                    .await,
                failure_signal,
            )
            .await;
        }
    };
    retry_performance_status_transition(
        state,
        operation_id,
        expected_state,
        expected_error,
        transition,
        failure_signal,
    )
    .await
}

pub(super) async fn retry_performance_status_transition(
    state: &AppState,
    operation_id: &OperationId,
    expected_state: &str,
    expected_error: Option<&str>,
    result: Result<(), PerformanceOperationStoreError>,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> Result<(), PerformanceOperationStoreError> {
    retry_performance_status_transition_inner(
        state,
        operation_id,
        expected_state,
        expected_error,
        result,
        failure_signal,
        None,
    )
    .await
}

pub(super) async fn retry_performance_status_correction(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    expected_error: &str,
    result: Result<(), PerformanceOperationStoreError>,
) -> Result<(), PerformanceOperationStoreError> {
    retry_performance_status_transition_inner(
        state,
        operation_id,
        "failed",
        Some(expected_error),
        result,
        None,
        Some(action),
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn retry_performance_status_transition_inner(
    state: &AppState,
    operation_id: &OperationId,
    expected_state: &str,
    expected_error: Option<&str>,
    mut result: Result<(), PerformanceOperationStoreError>,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
    reconciliation_correction: Option<PerformanceInstallAction>,
) -> Result<(), PerformanceOperationStoreError> {
    let mut delay_ms = PERFORMANCE_RETRY_INITIAL_DELAY_MS;
    loop {
        match result {
            Ok(()) => return Ok(()),
            Err(error @ PerformanceOperationStoreError::Persistence(_)) => {
                if let Some(signal) = failure_signal {
                    signal.notify();
                }
                if !state
                    .performance_operations()
                    .has_retry_candidate(operation_id.as_str())
                {
                    if performance_status_matches(
                        state,
                        operation_id,
                        expected_state,
                        expected_error,
                    )
                    .await
                    {
                        return Ok(());
                    }
                    return Err(error);
                }
                tracing::warn!(
                    operation_id = operation_id.as_str(),
                    status_error = error.class(),
                    "performance operation status commit failed; retrying"
                );
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                if !state
                    .performance_operations()
                    .has_retry_candidate(operation_id.as_str())
                {
                    if performance_status_matches(
                        state,
                        operation_id,
                        expected_state,
                        expected_error,
                    )
                    .await
                    {
                        return Ok(());
                    }
                    return Err(error);
                }
                delay_ms = delay_ms
                    .saturating_mul(2)
                    .min(PERFORMANCE_RETRY_MAX_DELAY_MS);
                match state
                    .performance_operations()
                    .retry_critical(operation_id.as_str())
                    .await
                {
                    Ok(()) => return Ok(()),
                    Err(PerformanceOperationStoreError::RetryUnavailable) => {
                        return if performance_status_matches(
                            state,
                            operation_id,
                            expected_state,
                            expected_error,
                        )
                        .await
                        {
                            Ok(())
                        } else {
                            Err(error)
                        };
                    }
                    Err(next @ PerformanceOperationStoreError::Persistence(_)) => {
                        result = Err(next);
                    }
                    Err(next) => return Err(next),
                }
            }
            Err(PerformanceOperationStoreError::RetryRequired) => {
                if let Some(signal) = failure_signal {
                    signal.notify();
                }
                while state
                    .performance_operations()
                    .has_retry_candidate(operation_id.as_str())
                {
                    tracing::warn!(
                        operation_id = operation_id.as_str(),
                        status_error = "retry_required",
                        "performance operation status commit is blocked; draining prior transition"
                    );
                    tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
                    delay_ms = delay_ms
                        .saturating_mul(2)
                        .min(PERFORMANCE_RETRY_MAX_DELAY_MS);
                    match state
                        .performance_operations()
                        .retry_critical(operation_id.as_str())
                        .await
                    {
                        Ok(()) | Err(PerformanceOperationStoreError::RetryUnavailable) => {}
                        Err(
                            PerformanceOperationStoreError::Persistence(_)
                            | PerformanceOperationStoreError::RetryRequired,
                        ) => continue,
                        Err(error) => return Err(error),
                    }
                }
                if performance_status_matches(state, operation_id, expected_state, expected_error)
                    .await
                {
                    return Ok(());
                }
                result = apply_expected_performance_status_transition(
                    state,
                    operation_id,
                    expected_state,
                    expected_error,
                    reconciliation_correction,
                )
                .await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn apply_expected_performance_status_transition(
    state: &AppState,
    operation_id: &OperationId,
    expected_state: &str,
    expected_error: Option<&str>,
    reconciliation_correction: Option<PerformanceInstallAction>,
) -> Result<(), PerformanceOperationStoreError> {
    match expected_state {
        "queued" => {
            state
                .performance_operations()
                .record_progress(operation_id.as_str(), "queued")
                .await
        }
        PERFORMANCE_EFFECT_STARTED_STATE => {
            state
                .performance_operations()
                .record_effect_started(operation_id.as_str())
                .await
        }
        PERFORMANCE_COMMITTING_COMPLETE_STATE => {
            state
                .performance_operations()
                .record_committing_complete(operation_id.as_str())
                .await
        }
        PERFORMANCE_COMMITTING_FAILED_STATE => {
            state
                .performance_operations()
                .record_committing_failed(
                    operation_id.as_str(),
                    expected_error.unwrap_or(PERFORMANCE_RECONCILIATION_FAILURE),
                )
                .await
        }
        "complete" => {
            state
                .performance_operations()
                .record_complete(operation_id.as_str())
                .await
        }
        "failed" if reconciliation_correction.is_some() => {
            let action = reconciliation_correction.expect("correction action is present");
            state
                .performance_operations()
                .record_reconciliation_failed(
                    operation_id.as_str(),
                    expected_error.unwrap_or(PERFORMANCE_RECONCILIATION_FAILURE),
                    operation_action_name(action),
                )
                .await
        }
        "failed" => {
            state
                .performance_operations()
                .record_failed(
                    operation_id.as_str(),
                    expected_error.unwrap_or(PERFORMANCE_RECONCILIATION_FAILURE),
                )
                .await
        }
        _ => Err(PerformanceOperationStoreError::TerminalMismatch),
    }
}

async fn performance_status_matches(
    state: &AppState,
    operation_id: &OperationId,
    expected_state: &str,
    expected_error: Option<&str>,
) -> bool {
    let expected_error = expected_error.map(sanitize_operation_error);
    state
        .performance_operations()
        .get(operation_id.as_str())
        .await
        .is_some_and(|status| status.state == expected_state && status.error == expected_error)
}

fn generated_performance_journal_operation_id(action: PerformanceInstallAction) -> OperationId {
    OperationId::new(format!(
        "performance-{}-{}",
        performance_operation_step_id(action),
        uuid::Uuid::new_v4()
    ))
}

pub(super) async fn record_performance_operation_result(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    fallback_target_id: &str,
    rollback: RollbackState,
    result: &Result<PerformanceInstallResponse, PerformanceApplicationError>,
    failure_signal: Option<&PerformancePersistenceFailureSignal>,
) -> Result<(), PerformanceOperationExecutionError> {
    let target_id = match result {
        Ok(response) => {
            let response_target_id = response.composition_id.trim();
            if response_target_id.is_empty() {
                fallback_target_id
            } else {
                response_target_id
            }
        }
        Err(_) => fallback_target_id,
    };
    record_performance_terminal_intent(
        state,
        operation_id,
        action,
        target_id,
        rollback,
        result.is_ok(),
    )
    .await
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(
            Some(operation_id.clone()),
            error,
            PerformanceJournalTransition::terminal_intent(
                action,
                fallback_target_id,
                target_id,
                rollback,
                result.is_ok(),
            ),
        )
    })?;
    record_performance_terminal_intent_status(state, operation_id, result, failure_signal).await?;
    match result {
        Ok(_) => {
            record_performance_operation_success(state, operation_id, action, target_id, rollback)
                .await
                .map_err(|error| {
                    PerformanceOperationExecutionError::journal_transition(
                        Some(operation_id.clone()),
                        error,
                        PerformanceJournalTransition::terminal(
                            action,
                            fallback_target_id,
                            target_id,
                            rollback,
                            true,
                        ),
                    )
                })
        }
        Err(_) => {
            record_performance_operation_failure(state, operation_id, action, target_id, rollback)
                .await
                .map_err(|error| {
                    PerformanceOperationExecutionError::journal_transition(
                        Some(operation_id.clone()),
                        error,
                        PerformanceJournalTransition::terminal(
                            action,
                            fallback_target_id,
                            target_id,
                            rollback,
                            false,
                        ),
                    )
                })
        }
    }
}

async fn record_performance_operation_success(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> Result<(), OperationJournalStoreError> {
    let rollback = if matches!(action, PerformanceInstallAction::Rollback) {
        RollbackState::Applied
    } else {
        rollback
    };
    state
        .journals()
        .record_success(
            operation_id,
            performance_operation_journal_step(
                action,
                OperationStepResult::Completed,
                target_id,
                rollback,
            ),
            OperationOutcome::Succeeded,
        )
        .await
}

pub(super) async fn record_performance_operation_failure(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> Result<(), OperationJournalStoreError> {
    state
        .journals()
        .record_failure(
            operation_id,
            performance_operation_journal_step(
                action,
                OperationStepResult::Failed,
                target_id,
                rollback,
            ),
            performance_operation_step_id(action),
            OperationOutcome::Failed,
        )
        .await
}

pub(super) async fn record_performance_guardian_supervision(
    state: &AppState,
    operation_id: &OperationId,
    supervision: &GuardianPerformanceSupervisionPlan,
) -> Result<(), OperationJournalStoreError> {
    state
        .journals()
        .record_guardian_evidence(
            operation_id,
            supervision
                .fact_ids
                .iter()
                .map(|fact_id| fact_id.as_str().to_string())
                .collect(),
            supervision.decision.diagnoses().to_vec(),
        )
        .await
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

fn performance_operation_start_error(
    error: PerformanceOperationStartError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        PerformanceOperationStartError::Conflict => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "a performance operation is already queued for this instance"
            })),
        ),
        PerformanceOperationStartError::Store { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
        ),
    }
}
