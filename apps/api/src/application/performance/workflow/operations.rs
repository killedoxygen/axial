use super::PerformanceInstallResponse;
use super::managed_plan::ManagedPlanResolutionError;
use super::mutation::{
    PerformanceJournalIdentity, execute_performance_operation_with_resolver_and_progress,
    performance_operation_journal_identity, resolve_performance_install_plan,
};
use crate::guardian::GuardianPerformanceSupervisionPlan;
use crate::observability::{
    OperationProofRecord, RedactionAudience, operation_journal_proof_record,
    sanitize_evidence_token, sanitize_public_diagnostic_text,
};
use crate::state::contracts::{
    OperationId, PerformanceOperationAction, PerformanceOperationIntent, PerformanceOperationPhase,
    PerformanceOperationPrepared, PerformanceOperationTerminal, PerformancePreparedProof,
    RollbackState,
};
use crate::state::{
    AppState, DownloadProgress, InstalledVersionsSnapshot, IntegrityForegroundLease,
    IntegrityForegroundRegistration, OperationJournalStoreError, PerformanceOperationCreateError,
    PerformanceOperationProjection, PerformanceOperationTransition, PerformanceRestartPlan,
    ProducerLease, RequestProducerHandoff,
};
use axial_performance::ManagedCompositionInstallPlan;
use axum::{Json, http::StatusCode};
use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use std::{
    future::Future,
    sync::{Arc, Mutex},
};

pub(super) const PERFORMANCE_JOURNAL_ERROR: &str =
    "Could not save performance operation safety state. Check app data permissions and try again.";
const PERFORMANCE_RECONCILIATION_FAILURE: &str =
    "performance operation outcome could not be confirmed after restart";
const PERFORMANCE_WORKER_INTERRUPTED_FAILURE: &str =
    "performance operation stopped before its result could be confirmed";
const PERFORMANCE_RESUME_REJECTED_FAILURE: &str =
    "performance operation could not be resumed before effect";
const MAX_OPERATION_ERROR_CHARS: usize = 160;

pub(super) type PerformanceApplicationError = (StatusCode, Json<serde_json::Value>);

pub(super) struct PerformanceOperationResultRequest<'a> {
    pub(super) operation_id: &'a OperationId,
    pub(super) terminal_rollback: RollbackState,
    pub(super) changed_target: bool,
    pub(super) result: &'a Result<PerformanceInstallResponse, PerformanceApplicationError>,
    pub(super) failure_signal: Option<&'a PerformancePersistenceFailureSignal>,
}

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
    Journal { error: OperationJournalStoreError },
}

impl PerformanceOperationExecutionError {
    pub(super) fn journal_transition(
        operation_id: Option<OperationId>,
        error: OperationJournalStoreError,
    ) -> Self {
        let _ = operation_id;
        Self::Journal { error }
    }

    #[cfg(test)]
    pub(super) fn into_application_error(self) -> PerformanceApplicationError {
        match self {
            Self::Operation(error) => error,
            Self::Journal { .. } => (
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

#[derive(Debug, Serialize)]
pub struct PerformanceInstanceOperationResponse {
    pub operation: Option<PerformanceOperationStatusResponse>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PerformanceOperationPayload {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub game_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub loader: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rollback_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct PerformanceOperationStatus {
    pub id: OperationId,
    pub instance_id: String,
    pub action: String,
    pub payload: PerformanceOperationPayload,
    pub state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
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
    pub(super) status_operation_id: Option<OperationId>,
    pub(super) resume_existing_journal: bool,
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
    operation_id: Arc<Mutex<Option<OperationId>>>,
}

impl PerformanceWorkerIdentity {
    pub(super) fn set(&self, operation_id: OperationId) {
        *self
            .operation_id
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(operation_id);
    }

    pub(super) fn get(&self) -> Option<OperationId> {
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
    let Some(projection) = state.journals().performance_operation(&install_id) else {
        return;
    };
    if projection.terminal {
        return;
    }
    if let Err(error) = settle_interrupted_performance_operation(&state, &projection).await {
        tracing::error!(
            operation_id = %install_id,
            action = operation_action_name(action),
            interrupted,
            journal_error = error.class(),
            "performance operation worker stopped before settlement"
        );
    }
}

async fn settle_interrupted_performance_operation(
    state: &AppState,
    projection: &PerformanceOperationProjection,
) -> Result<(), OperationJournalStoreError> {
    match &projection.phase {
        PerformanceOperationPhase::Accepted {} | PerformanceOperationPhase::Planning {} => {
            let terminal = PerformanceOperationTerminal::FailedBeforeEffect {
                error: PERFORMANCE_WORKER_INTERRUPTED_FAILURE.to_string(),
            };
            state
                .journals()
                .transition_performance(
                    &projection.operation_id,
                    PerformanceOperationTransition::RequestTerminal(terminal.clone()),
                )
                .await?;
            state
                .journals()
                .transition_performance(
                    &projection.operation_id,
                    PerformanceOperationTransition::CommitTerminal(terminal),
                )
                .await
        }
        PerformanceOperationPhase::Prepared { .. } => Ok(()),
        PerformanceOperationPhase::EffectStarted { .. } => {
            state
                .journals()
                .transition_performance(
                    &projection.operation_id,
                    PerformanceOperationTransition::MarkAppliedUnverified(
                        PERFORMANCE_WORKER_INTERRUPTED_FAILURE.to_string(),
                    ),
                )
                .await
        }
        PerformanceOperationPhase::AppliedUnverified { .. }
        | PerformanceOperationPhase::Terminal { .. } => Ok(()),
        PerformanceOperationPhase::TerminalIntent { terminal } => {
            state
                .journals()
                .transition_performance(
                    &projection.operation_id,
                    PerformanceOperationTransition::CommitTerminal(terminal.clone()),
                )
                .await
        }
    }
}

pub(super) async fn settle_performance_application_error(
    state: &AppState,
    operation_id: &OperationId,
    error: &PerformanceApplicationError,
) -> Result<(), OperationJournalStoreError> {
    let projection = state
        .journals()
        .performance_operation(operation_id)
        .ok_or(OperationJournalStoreError::MissingOperation)?;
    if !matches!(
        &projection.phase,
        PerformanceOperationPhase::Accepted {}
            | PerformanceOperationPhase::Planning {}
            | PerformanceOperationPhase::Prepared { .. }
    ) {
        return Ok(());
    }
    let terminal = PerformanceOperationTerminal::FailedBeforeEffect {
        error: error_message(error),
    };
    state
        .journals()
        .transition_performance(
            operation_id,
            PerformanceOperationTransition::RequestTerminal(terminal.clone()),
        )
        .await?;
    state
        .journals()
        .transition_performance(
            operation_id,
            PerformanceOperationTransition::CommitTerminal(terminal),
        )
        .await
}

pub(super) async fn settle_performance_journal_rejection(
    state: &AppState,
    projection: &PerformanceOperationProjection,
    error: &OperationJournalStoreError,
) -> Result<(), OperationJournalStoreError> {
    if matches!(error, OperationJournalStoreError::AlreadyExists)
        && matches!(
            &projection.phase,
            PerformanceOperationPhase::Prepared { .. }
        )
    {
        let terminal = PerformanceOperationTerminal::FailedBeforeEffect {
            error: PERFORMANCE_RESUME_REJECTED_FAILURE.to_string(),
        };
        state
            .journals()
            .transition_performance(
                &projection.operation_id,
                PerformanceOperationTransition::RequestTerminal(terminal.clone()),
            )
            .await?;
        return state
            .journals()
            .transition_performance(
                &projection.operation_id,
                PerformanceOperationTransition::CommitTerminal(terminal),
            )
            .await;
    }
    settle_interrupted_performance_operation(state, projection).await
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
    let id = OperationId::try_from(id).map_err(|_| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "performance operation not found" })),
        )
    })?;
    state
        .journals()
        .performance_operation(&id)
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
        .journals()
        .current_or_latest_performance_operation(&instance.id)
        .map(|status| public_performance_operation_status(state, status));

    Ok(PerformanceInstanceOperationResponse { operation })
}

pub(super) struct PerformanceOperationBootstrapError {
    pub(super) response: PerformanceApplicationError,
}

pub(super) async fn bootstrap_performance_operation(
    state: &AppState,
    operation: &mut PerformanceOperation,
    worker_identity: &PerformanceWorkerIdentity,
    runtime_owner: &ProducerLease,
    foreground: &IntegrityForegroundLease,
) -> Result<OperationId, PerformanceOperationBootstrapError> {
    operation.installed_versions =
        stage_performance_installed_versions(state, operation, runtime_owner, foreground).await;
    let identity = durable_performance_operation_identity(state, operation, foreground)
        .await
        .map_err(|response| PerformanceOperationBootstrapError { response })?;
    let intent = performance_operation_intent(operation, identity);
    let projection = match state.journals().create_performance(intent.clone()).await {
        Ok(projection) => projection,
        Err(PerformanceOperationCreateError::BeforeAdmission(error)) => {
            return Err(PerformanceOperationBootstrapError {
                response: performance_operation_start_error(&error),
            });
        }
        Err(PerformanceOperationCreateError::AfterAdmission {
            operation_id,
            source,
        }) => {
            worker_identity.set(operation_id.clone());
            operation.status_operation_id = Some(operation_id.clone());
            let _ = state
                .installs()
                .admit(operation_id.to_string(), operation_id.clone())
                .await;
            let response = performance_operation_start_error(&source);
            let retained_state = state.clone();
            let retained_id = operation_id.clone();
            runtime_owner.spawn_child(async move {
                let reconciled = retained_state
                    .journals()
                    .reconcile_performance_create(&retained_id, &intent, source)
                    .await;
                if let Err(error) = reconciled {
                    tracing::warn!(
                        operation_id = %retained_id,
                        journal_error = error.class(),
                        "admitted performance operation could not be reconciled"
                    );
                    return;
                }
                let Some(projection) = retained_state
                    .journals()
                    .performance_operation(&retained_id)
                else {
                    return;
                };
                if let Err(error) =
                    settle_interrupted_performance_operation(&retained_state, &projection).await
                {
                    tracing::warn!(
                        operation_id = %retained_id,
                        journal_error = error.class(),
                        "admitted performance operation could not be terminalized"
                    );
                    return;
                }
                if let Some(projection) = retained_state
                    .journals()
                    .performance_operation(&retained_id)
                {
                    publish_performance_projection(retained_state.installs(), &projection, None)
                        .await;
                }
            });
            return Err(PerformanceOperationBootstrapError { response });
        }
    };
    let install_id = projection.operation_id;
    operation.action = application_performance_action(projection.intent.action);
    worker_identity.set(install_id.clone());
    operation.status_operation_id = Some(install_id.clone());
    let _ = state
        .installs()
        .admit(install_id.to_string(), install_id.clone())
        .await;
    Ok(install_id)
}

pub(super) async fn queue_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    handoff: RequestProducerHandoff,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let (ownership_tx, ownership_rx) = tokio::sync::oneshot::channel();
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(|_| performance_shutdown_error())?;
    let foreground = register_performance_foreground(&state)?;
    let worker_owner = producer.claim_child();
    let worker_identity = PerformanceWorkerIdentity::default();
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
                let install_id = match bootstrap_performance_operation(
                    &state,
                    &mut operation,
                    &worker_identity,
                    &runtime_owner,
                    &worker_foreground,
                )
                .await
                {
                    Ok(install_id) => install_id,
                    Err(error) => {
                        let _ = ownership_tx.send(Err(error.response));
                        return;
                    }
                };
                let store = state.installs().clone();
                let response = PerformanceInstallResponse {
                    active: true,
                    status: "queued".to_string(),
                    install_id: Some(install_id.to_string()),
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
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(|_| performance_shutdown_error())?;
    let foreground = register_performance_foreground(&state)?;
    let worker_owner = producer.claim_child();
    let worker_identity = PerformanceWorkerIdentity::default();
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
                let install_id = match bootstrap_performance_operation(
                    &state,
                    &mut operation,
                    &worker_identity,
                    &runtime_owner,
                    &worker_foreground,
                )
                .await
                {
                    Ok(install_id) => install_id,
                    Err(error) => {
                        let _ = completion_tx.send(Err(error.response));
                        return;
                    }
                };
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

pub(super) async fn resume_pending_performance_operations_owned(
    state: AppState,
    producer: &ProducerLease,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> usize {
    let PerformanceRestartPlan {
        resumable,
        applied_unverified,
    } = match state.journals().settle_performance_restarts().await {
        Ok(plan) => plan,
        Err(error) => {
            tracing::warn!(
                journal_error = error.class(),
                "performance restart settlement failed"
            );
            return 0;
        }
    };
    for projection in applied_unverified {
        tracing::warn!(
            operation_id = %projection.operation_id,
            "performance operation requires applied-unverified reconciliation"
        );
    }
    if resumable.is_empty() {
        return 0;
    }
    let Ok(foreground) = state.register_integrity_foreground() else {
        return 0;
    };
    let foreground = foreground.wait_for_settlement().await;
    let mut resumed = 0usize;
    for status in resumable {
        if *shutdown.borrow_and_update() {
            return resumed;
        }
        resumed = resumed.saturating_add(1);
        let install_id = status.operation_id.clone();
        let _ = state
            .installs()
            .admit(install_id.to_string(), install_id.clone())
            .await;
        let store = state.installs().clone();
        let mut operation = operation_from_projection(&status);
        let action = operation.action;
        let worker_identity = PerformanceWorkerIdentity::default();
        worker_identity.set(install_id.clone());
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

pub(super) async fn run_queued_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: OperationId,
    foreground: IntegrityForegroundLease,
) {
    run_owned_performance_operation(state, operation, store, install_id, None, &foreground).await;
}

async fn run_owned_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: OperationId,
    completion: Option<
        tokio::sync::oneshot::Sender<
            Result<PerformanceInstallResponse, PerformanceApplicationError>,
        >,
    >,
    foreground: &IntegrityForegroundLease,
) {
    run_owned_performance_operation_with_resolver(
        state,
        operation,
        store,
        install_id,
        completion,
        foreground,
        resolve_performance_install_plan,
    )
    .await;
}

async fn run_owned_performance_operation_with_resolver<Resolver, ResolutionFuture>(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: OperationId,
    mut completion: Option<
        tokio::sync::oneshot::Sender<
            Result<PerformanceInstallResponse, PerformanceApplicationError>,
        >,
    >,
    foreground: &IntegrityForegroundLease,
    resolver: Resolver,
) where
    Resolver: Fn(AppState, axial_performance::CompositionPlan, String, String) -> ResolutionFuture
        + Clone,
    ResolutionFuture:
        Future<Output = Result<ManagedCompositionInstallPlan, ManagedPlanResolutionError>>,
{
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
    if let Err(error) = state
        .journals()
        .transition_performance(&install_id, PerformanceOperationTransition::Planning)
        .await
    {
        if let Some(signal) = &operation.persistence_failure {
            signal.notify();
        }
        tracing::warn!(operation_id = %install_id, journal_error = error.class(), "performance planning transition was rejected");
        send_performance_completion_error(&mut completion);
        return;
    }
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

    let progress_store = store.clone();
    let progress_id = install_id.clone();
    let result = execute_performance_operation_with_resolver_and_progress(
        &state,
        &operation,
        foreground,
        resolver,
        move |action| async move {
            let phase = operation_progress_phase(action);
            emit_performance_progress(
                &progress_store,
                &progress_id,
                phase,
                2,
                4,
                Some(operation_progress_label(action)),
                None,
                false,
            )
            .await;
        },
    )
    .await;

    match result {
        Ok(response) => {
            let published = match state.journals().performance_operation(&install_id) {
                Some(projection) => {
                    publish_performance_projection(
                        &store,
                        &projection,
                        Some(complete_progress_label(&response.status)),
                    )
                    .await
                }
                None => false,
            };
            if published {
                if let Some(completion) = completion.take() {
                    let _ = completion.send(Ok(response));
                }
            } else {
                send_performance_completion_error(&mut completion);
            }
        }
        Err(PerformanceOperationExecutionError::Operation(error)) => {
            if let Err(journal_error) =
                settle_performance_application_error(&state, &install_id, &error).await
            {
                tracing::warn!(operation_id = %install_id, journal_error = journal_error.class(), "pre-effect performance failure could not be terminalized");
            }
            let published = match state.journals().performance_operation(&install_id) {
                Some(projection) => publish_performance_projection(&store, &projection, None).await,
                None => false,
            };
            if published {
                if let Some(completion) = completion.take() {
                    let _ = completion.send(Err(error));
                }
            } else {
                send_performance_completion_error(&mut completion);
            }
        }
        Err(PerformanceOperationExecutionError::Journal { error, .. }) => {
            if let Some(signal) = &operation.persistence_failure {
                signal.notify();
            }
            tracing::warn!(operation_id = %install_id, journal_error = error.class(), "performance journal transition was rejected");
            send_performance_completion_error(&mut completion);
            if let Some(projection) = state.journals().performance_operation(&install_id) {
                let _ = settle_performance_journal_rejection(&state, &projection, &error).await;
                if let Some(projection) = state.journals().performance_operation(&install_id) {
                    publish_performance_projection(&store, &projection, None).await;
                }
            }
        }
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
#[allow(clippy::too_many_arguments)]
async fn emit_performance_progress(
    store: &crate::state::InstallStore,
    install_id: &OperationId,
    phase: &str,
    current: i32,
    total: i32,
    file: Option<&str>,
    error: Option<String>,
    done: bool,
) {
    store
        .emit(
            &install_id.to_string(),
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
) -> Result<PerformanceJournalIdentity, PerformanceApplicationError> {
    performance_operation_journal_identity(state, operation, foreground).await
}

pub(super) fn performance_operation_intent(
    operation: &PerformanceOperation,
    identity: PerformanceJournalIdentity,
) -> PerformanceOperationIntent {
    let payload = operation_payload(operation);
    PerformanceOperationIntent {
        instance_id: operation.instance_id.clone(),
        requested_action: state_performance_action(operation.action),
        action: state_performance_action(identity.action),
        base_target_id: identity.target_id,
        rollback: identity.rollback,
        game_version: payload.game_version,
        loader: payload.loader,
        mode: payload.mode,
        rollback_id: payload.rollback_id,
    }
}

fn public_performance_operation_status(
    state: &AppState,
    status: PerformanceOperationProjection,
) -> PerformanceOperationStatusResponse {
    let proof = performance_operation_proof(state, &status);
    let payload = PerformanceOperationPayload {
        game_version: status.intent.game_version,
        loader: status.intent.loader,
        mode: status.intent.mode,
        rollback_id: status.intent.rollback_id,
    };
    let status = PerformanceOperationStatus {
        id: status.operation_id,
        instance_id: public_operation_required_token(&status.intent.instance_id, "redacted"),
        action: operation_action_name(application_performance_action(
            status.intent.requested_action,
        ))
        .into(),
        payload: public_operation_payload(payload),
        state: status.state.to_string(),
        error: status
            .error
            .as_deref()
            .map(sanitize_operation_error)
            .filter(|value| !value.trim().is_empty()),
        created_at: public_operation_timestamp(&status.created_at),
        updated_at: public_operation_timestamp(&status.updated_at),
    };
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
    let attention = matches!(state, "failed" | "interrupted" | "applied_unverified");
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
        tone: if attention {
            "err"
        } else if is_complete {
            "ok"
        } else {
            "mute"
        },
        title: operation_status_title(state),
        detail: if attention {
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
    if matches!(state, "failed" | "interrupted" | "applied_unverified") {
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
    status: &PerformanceOperationProjection,
) -> Option<OperationProofRecord> {
    if !status.terminal {
        return None;
    }
    state.journals().get(&status.operation_id).map(|journal| {
        let mut proof = operation_journal_proof_record(&journal);
        proof.rollback = match &status.phase {
            PerformanceOperationPhase::Terminal {
                terminal:
                    PerformanceOperationTerminal::Succeeded { rollback, .. }
                    | PerformanceOperationTerminal::FailedAfterEffect { rollback, .. },
            } => *rollback,
            _ => status.intent.rollback,
        };
        Some(proof)
    })?
}

fn performance_status_is_terminal(status: &str) -> bool {
    matches!(status, "complete" | "failed" | "interrupted")
}

fn public_operation_required_token(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn public_operation_timestamp(value: &str) -> String {
    normalized_operation_timestamp(value).unwrap_or_else(|| "unknown".to_string())
}

fn normalized_operation_timestamp(value: &str) -> Option<String> {
    DateTime::parse_from_rfc3339(value.trim())
        .ok()
        .map(|value| {
            value
                .with_timezone(&Utc)
                .to_rfc3339_opts(SecondsFormat::AutoSi, true)
        })
}

fn sanitize_operation_error(value: &str) -> String {
    sanitize_public_diagnostic_text(
        value,
        RedactionAudience::UserVisible,
        MAX_OPERATION_ERROR_CHARS,
        "performance operation failed",
    )
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

pub(super) fn operation_from_projection(
    status: &PerformanceOperationProjection,
) -> PerformanceOperation {
    PerformanceOperation {
        instance_id: status.intent.instance_id.clone(),
        game_version: status.intent.game_version.clone(),
        loader: status.intent.loader.clone(),
        mode: status.intent.mode.clone(),
        action: application_performance_action(status.intent.action),
        rollback_id: status.intent.rollback_id.clone(),
        status_operation_id: Some(status.operation_id.clone()),
        resume_existing_journal: true,
        persistence_failure: None,
        installed_versions: None,
    }
}

fn state_performance_action(action: PerformanceInstallAction) -> PerformanceOperationAction {
    match action {
        PerformanceInstallAction::Install => PerformanceOperationAction::Install,
        PerformanceInstallAction::Remove => PerformanceOperationAction::Remove,
        PerformanceInstallAction::Rollback => PerformanceOperationAction::Rollback,
    }
}

fn application_performance_action(action: PerformanceOperationAction) -> PerformanceInstallAction {
    match action {
        PerformanceOperationAction::Install => PerformanceInstallAction::Install,
        PerformanceOperationAction::Remove => PerformanceInstallAction::Remove,
        PerformanceOperationAction::Rollback => PerformanceInstallAction::Rollback,
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

async fn publish_performance_projection(
    store: &crate::state::InstallStore,
    projection: &PerformanceOperationProjection,
    complete_label: Option<&str>,
) -> bool {
    match &projection.phase {
        PerformanceOperationPhase::Terminal {
            terminal: PerformanceOperationTerminal::Succeeded { .. },
        } => {
            let action = application_performance_action(projection.intent.action);
            emit_performance_progress(
                store,
                &projection.operation_id,
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
        PerformanceOperationPhase::Terminal { .. } => {
            let message = sanitize_operation_error(
                projection
                    .error
                    .as_deref()
                    .unwrap_or(PERFORMANCE_RECONCILIATION_FAILURE),
            );
            emit_performance_progress(
                store,
                &projection.operation_id,
                "error",
                4,
                4,
                None,
                Some(message),
                true,
            )
            .await;
            true
        }
        PerformanceOperationPhase::Accepted {}
        | PerformanceOperationPhase::Planning {}
        | PerformanceOperationPhase::Prepared { .. }
        | PerformanceOperationPhase::EffectStarted { .. }
        | PerformanceOperationPhase::AppliedUnverified { .. }
        | PerformanceOperationPhase::TerminalIntent { .. } => false,
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
    linked_operation_id: Option<&OperationId>,
    _allow_existing: bool,
) -> Result<OperationId, OperationJournalStoreError> {
    let operation_id = linked_operation_id
        .cloned()
        .ok_or(OperationJournalStoreError::MissingOperation)?;
    let projection = state
        .journals()
        .performance_operation(&operation_id)
        .ok_or(OperationJournalStoreError::MissingOperation)?;
    if projection.intent.action != state_performance_action(action)
        || projection.intent.base_target_id != target_id
        || projection.intent.rollback != rollback
        || projection.terminal
    {
        return Err(OperationJournalStoreError::AlreadyExists);
    }
    Ok(operation_id)
}

pub(super) async fn record_performance_effect_started(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    _rollback: RollbackState,
) -> Result<(), OperationJournalStoreError> {
    let projection = state
        .journals()
        .performance_operation(operation_id)
        .ok_or(OperationJournalStoreError::MissingOperation)?;
    if projection.intent.action != state_performance_action(action)
        || projection.intent.base_target_id != target_id
        || !matches!(projection.phase, PerformanceOperationPhase::Prepared { .. })
    {
        return Err(OperationJournalStoreError::AlreadyExists);
    }
    state
        .journals()
        .transition_performance(operation_id, PerformanceOperationTransition::EffectStarted)
        .await
}

pub(super) async fn record_performance_plan_resolved(
    state: &AppState,
    operation_id: &OperationId,
    _action: PerformanceInstallAction,
    target_id: &str,
    _rollback: RollbackState,
    plan: &ManagedCompositionInstallPlan,
) -> Result<(), OperationJournalStoreError> {
    let artifact_count = u64::try_from(plan.pins().len())
        .map_err(|_| OperationJournalStoreError::CapacityExhausted)?;
    state
        .journals()
        .transition_performance(
            operation_id,
            PerformanceOperationTransition::Prepared(PerformanceOperationPrepared {
                result_target_id: target_id.to_string(),
                proof: PerformancePreparedProof::InstallPlan {
                    graph_sha512: plan.graph_digest().to_string(),
                    artifact_count,
                    aggregate_bytes: plan.aggregate_bytes(),
                },
            }),
        )
        .await
}

pub(super) async fn record_performance_prepared(
    state: &AppState,
    operation_id: &OperationId,
    prepared: PerformanceOperationPrepared,
) -> Result<(), OperationJournalStoreError> {
    state
        .journals()
        .transition_performance(
            operation_id,
            PerformanceOperationTransition::Prepared(prepared),
        )
        .await
}

pub(super) async fn record_performance_applied_unverified(
    state: &AppState,
    operation_id: &OperationId,
    error: &str,
) -> Result<(), OperationJournalStoreError> {
    state
        .journals()
        .transition_performance(
            operation_id,
            PerformanceOperationTransition::MarkAppliedUnverified(error.to_string()),
        )
        .await
}

fn performance_terminal_for_result(
    projection: &PerformanceOperationProjection,
    succeeded: bool,
    terminal_rollback: RollbackState,
    changed_target: bool,
    error: &str,
) -> Result<PerformanceOperationTerminal, OperationJournalStoreError> {
    let (prepared, effect_started) = match &projection.phase {
        PerformanceOperationPhase::Prepared { prepared } => (Some(prepared.clone()), false),
        PerformanceOperationPhase::EffectStarted { prepared }
        | PerformanceOperationPhase::AppliedUnverified { prepared, .. } => {
            (Some(prepared.clone()), true)
        }
        PerformanceOperationPhase::TerminalIntent { terminal }
        | PerformanceOperationPhase::Terminal { terminal } => return Ok(terminal.clone()),
        PerformanceOperationPhase::Accepted {} | PerformanceOperationPhase::Planning {} => {
            (None, false)
        }
    };
    match (succeeded, prepared, effect_started) {
        (true, Some(prepared), _) => Ok(PerformanceOperationTerminal::Succeeded {
            prepared,
            changed_target,
            rollback: terminal_rollback,
        }),
        (true, None, _) => Err(OperationJournalStoreError::AlreadyExists),
        (false, Some(prepared), true) => Ok(PerformanceOperationTerminal::FailedAfterEffect {
            prepared,
            changed_target,
            rollback: terminal_rollback,
            error: error.to_string(),
        }),
        (false, _, _) => Ok(PerformanceOperationTerminal::FailedBeforeEffect {
            error: error.to_string(),
        }),
    }
}

pub(super) async fn record_performance_operation_result(
    state: &AppState,
    request: PerformanceOperationResultRequest<'_>,
) -> Result<(), PerformanceOperationExecutionError> {
    let PerformanceOperationResultRequest {
        operation_id,
        terminal_rollback,
        changed_target,
        result,
        failure_signal,
        ..
    } = request;
    let projection = state
        .journals()
        .performance_operation(operation_id)
        .ok_or_else(|| {
            PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                OperationJournalStoreError::MissingOperation,
            )
        })?;
    let error = result
        .as_ref()
        .err()
        .map(error_message)
        .unwrap_or_else(|| PERFORMANCE_RECONCILIATION_FAILURE.to_string());
    let terminal = performance_terminal_for_result(
        &projection,
        result.is_ok(),
        terminal_rollback,
        changed_target,
        &error,
    )
    .map_err(|error| {
        PerformanceOperationExecutionError::journal_transition(Some(operation_id.clone()), error)
    })?;
    for transition in [
        PerformanceOperationTransition::RequestTerminal(terminal.clone()),
        PerformanceOperationTransition::CommitTerminal(terminal),
    ] {
        if let Err(error) = state
            .journals()
            .transition_performance(operation_id, transition)
            .await
        {
            if let Some(signal) = failure_signal {
                signal.notify();
            }
            return Err(PerformanceOperationExecutionError::journal_transition(
                Some(operation_id.clone()),
                error,
            ));
        }
    }
    Ok(())
}

pub(super) async fn record_performance_guardian_supervision(
    state: &AppState,
    operation_id: &OperationId,
    supervision: &GuardianPerformanceSupervisionPlan,
) -> Result<(), OperationJournalStoreError> {
    state
        .journals()
        .record_performance_guardian_evidence(
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
    error: &OperationJournalStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        OperationJournalStoreError::Conflict => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({
                "error": "a performance operation is already queued for this instance"
            })),
        ),
        OperationJournalStoreError::SequenceExhausted => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Could not allocate an operation identity. Try again."
            })),
        ),
        _ => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({ "error": PERFORMANCE_JOURNAL_ERROR })),
        ),
    }
}
