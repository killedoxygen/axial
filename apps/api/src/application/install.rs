//! Application-owned install orchestration facade.
//!
//! The facade owns request/response contracts, vanilla install worker
//! coordination, and status composition. Child modules own loader workflows,
//! operation journal/progress mapping, Guardian repair mapping, and event
//! streaming. Core Minecraft code still owns provider resolution, download
//! verification, and concrete install effects.

mod loader;
mod model;
mod operation;
mod repair;
mod stream;

#[cfg(test)]
pub(crate) use operation::loader_install_guardian_evidence_kind;

use super::InstallVersionCommand;
use crate::guardian::{
    DiagnosisId, GuardianArtifactRepairOutcome, GuardianInstallArtifactFailureEvidence,
    GuardianInstallOutcomeSummary,
};
use crate::observability::{
    operation_journal_proof_record,
    telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
    },
};
use crate::state::contracts::{OperationId, OperationJournalEntry};
use crate::state::{
    ActiveQueuedInstallEntry, InstallInitializationStatus, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallStore,
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    QueuedInstallEntry, operation_journal_plan_is_visible,
};
use crate::state::{AppState, ProducerLease, RequestProducerHandoff};
use axial_minecraft::{
    DownloadError, DownloadProgress, Downloader,
    download::{ExecutionDownloadFact, SelectedDownloadArtifactDescriptor},
    resolve_build_record_for_install,
};
use axum::{Json, http::StatusCode};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

pub(crate) const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";
pub(crate) const LOADER_INSTALL_INTERRUPTED_MESSAGE: &str =
    "Loader install stopped before completing. Try again.";
pub(crate) const BASE_INSTALL_FAILED_MESSAGE: &str =
    "Base game install failed. Retry the install from Downloads.";
const INSTALL_JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(10);
const INSTALL_JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);

#[derive(Clone, Copy)]
pub(super) enum InstallJournalReconciliation {
    MutationCommitted,
    RetryMutation,
}

struct InstallInitializationReservation {
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: Option<String>,
    operation_id: OperationId,
    cleanup_owner: Option<ProducerLease>,
}

impl InstallInitializationReservation {
    fn new(
        store: Arc<InstallStore>,
        journals: Arc<OperationJournalStore>,
        install_id: String,
        operation_id: OperationId,
        cleanup_owner: ProducerLease,
    ) -> Self {
        Self {
            store,
            journals,
            install_id: Some(install_id),
            operation_id,
            cleanup_owner: Some(cleanup_owner),
        }
    }

    fn hand_off(mut self) {
        self.install_id = None;
    }
}

impl Drop for InstallInitializationReservation {
    fn drop(&mut self) {
        let Some(install_id) = self.install_id.take() else {
            return;
        };
        let store = self.store.clone();
        let journals = self.journals.clone();
        let operation_id = self.operation_id.clone();
        let cleanup_owner = self
            .cleanup_owner
            .take()
            .expect("install initialization cleanup owner remains available");
        cleanup_owner.spawn(async move {
            let _ = store.mark_initialization_reconciling(&install_id).await;
            let _ = operation::record_install_operation_initialization_cancelled(
                &journals,
                &operation_id,
            )
            .await;
            store.remove(&install_id).await;
        });
    }
}

pub(super) async fn reconcile_install_journal_transition(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    error: OperationJournalStoreError,
    expected: impl Fn(&OperationJournalEntry) -> bool,
) -> Result<InstallJournalReconciliation, OperationJournalStoreError> {
    match journals
        .reconcile_transition(
            operation_id,
            error,
            INSTALL_JOURNAL_RETRY_INITIAL_DELAY,
            INSTALL_JOURNAL_RETRY_MAX_DELAY,
            expected,
        )
        .await?
    {
        OperationJournalReconciliation::CommittedAfterPersistenceFailure(_)
        | OperationJournalReconciliation::RequestedTransitionAlreadyCommitted => {
            Ok(InstallJournalReconciliation::MutationCommitted)
        }
        OperationJournalReconciliation::RetryRequestedTransition => {
            Ok(InstallJournalReconciliation::RetryMutation)
        }
    }
}

async fn begin_install_journal_with_owned_reconciliation(
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: String,
    operation_id: OperationId,
    version_id: String,
    producer: &ProducerLease,
) -> Result<InstallInitializationReservation, ()> {
    let reservation = InstallInitializationReservation::new(
        store,
        journals.clone(),
        install_id,
        operation_id.clone(),
        producer.claim_child(),
    );
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.claim_child().spawn(async move {
        match begin_install_operation_journal(&journals, &operation_id, &version_id).await {
            Ok(()) => {
                let _ = result_tx.send(Ok(reservation));
            }
            Err(error) => {
                let retryable = matches!(
                    &error,
                    OperationJournalStoreError::Persistence(_)
                        | OperationJournalStoreError::RetryRequired
                );
                if retryable {
                    let _ = reservation
                        .store
                        .mark_initialization_reconciling(
                            reservation.install_id.as_deref().unwrap_or_default(),
                        )
                        .await;
                }
                let _ = result_tx.send(Err(()));
                if !retryable {
                    return;
                }
                let expected = operation::planned_install_journal(&operation_id, &version_id);
                let mut error = error;
                loop {
                    match reconcile_install_journal_transition(
                        &journals,
                        &operation_id,
                        error,
                        |entry| operation_journal_plan_is_visible(entry, &expected),
                    )
                    .await
                    {
                        Ok(InstallJournalReconciliation::MutationCommitted) => return,
                        Ok(InstallJournalReconciliation::RetryMutation) => {}
                        Err(_) => return,
                    }
                    match begin_install_operation_journal(&journals, &operation_id, &version_id)
                        .await
                    {
                        Ok(()) => return,
                        Err(next_error) => error = next_error,
                    }
                }
            }
        }
    });
    result_rx.await.unwrap_or(Err(()))
}

#[cfg(test)]
async fn begin_install_journal_with_detached_reconciliation(
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: String,
    operation_id: OperationId,
    version_id: String,
) -> Result<InstallInitializationReservation, ()> {
    let lifecycle = crate::state::AppLifecycle::new();
    let producer = lifecycle
        .try_claim_producer()
        .expect("claim test install reconciliation");
    begin_install_journal_with_owned_reconciliation(
        store,
        journals,
        install_id,
        operation_id,
        version_id,
        &producer,
    )
    .await
}

pub type InstallApplicationError = (StatusCode, Json<serde_json::Value>);

pub(crate) struct ContinuationInstallQueueResult {
    pub response: InstallQueueStateResponse,
    pub outcome: InstallQueueEnqueueOutcome,
}

struct InstallQueueSelection {
    notice: InstallQueueNoticeViewModel,
    outcome: InstallQueueEnqueueOutcome,
}

use loader::start_loader_install_owned;
#[cfg(test)]
use loader::{
    dispatch_loader_install_failure, loader_install_done_progress, loader_install_error_progress,
    publish_known_good_loader_terminal, require_exact_loader_receipt_version,
    wait_for_active_vanilla_base_install,
};
pub use loader::{
    loader_builds, loader_components, loader_game_versions, loader_pre_operation_error_response,
};
pub use model::{
    InstallActionViewModel, InstallFailureViewModel, InstallGuardianRepairSummary,
    InstallProgressStepViewModel, InstallProgressViewModel, InstallQueueActiveViewModel,
    InstallQueueInstallItemViewModel, InstallQueueLoaderItemViewModel, InstallQueueNoticeViewModel,
    InstallQueueRequest, InstallQueueStateResponse, InstallQueueViewModel,
    InstallQueuedItemViewModel, InstallStartResponse, InstallStatusResponse, InstallVersionStaging,
    InstallVersionStartRequest, LoaderBuildsRequest, LoaderInstallStartRequest,
};
use operation::{
    InstallProgressCoalescer, assess_install_guardian_failure,
    install_failure_evidence_from_download_error_or_facts,
    install_failure_evidence_from_download_facts, install_failure_point_from_journal,
    install_journal_is_terminal, install_progress_history_from_journal, install_progress_record,
    install_progress_with_terminal_error, interrupted_install_progress,
    observed_install_failure_progress, public_install_id, record_install_guardian_terminal_outcome,
    record_install_operation_guardian_evidence,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome,
};
pub use operation::{
    begin_install_operation_journal, install_guardian_outcome_summary_from_journal,
    install_operation_id, loader_install_progress_view_model, public_loader_install_progress_json,
    public_vanilla_install_progress_json, record_install_operation_interrupted,
    record_install_operation_progress, sanitize_install_progress, stage_install_version_command,
    vanilla_install_progress_view_model,
};
use repair::{InstallRepairResume, repair_install_artifact_corruption_with_guardian};
pub use repair::{
    install_guardian_repair_summary_from_journal, record_install_operation_guardian_repair_outcome,
};
pub use stream::{install_events_stream, loader_install_events_stream};

pub(crate) async fn start_install_version_owned(
    state: &AppState,
    request: InstallVersionStartRequest,
    producer: &ProducerLease,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let version_id = effective_install_version_id(&request);
    if version_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        ));
    }

    let mc_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )
    })?;

    let install_id = loop {
        let candidate = generate_install_id("install");
        let (install_id, inserted) = state
            .installs()
            .insert_or_existing_vanilla(candidate, version_id.clone())
            .await;
        if inserted {
            break install_id;
        }
        match state.installs().wait_for_initialization(&install_id).await {
            InstallInitializationStatus::Initialized => {
                return Ok(InstallStartResponse {
                    operation_id: install_operation_id(&install_id),
                    install_id,
                    view_model: InstallProgressViewModel::starting(),
                });
            }
            InstallInitializationStatus::Reconciling => {
                return Err(install_journal_error_response());
            }
            InstallInitializationStatus::Removed => {}
        }
    };
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: version_id.clone(),
        },
        install_id.clone(),
        operation_id.clone(),
    );
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let reservation = begin_install_journal_with_owned_reconciliation(
        store.clone(),
        journals.clone(),
        install_id.clone(),
        operation_id.clone(),
        version_id.clone(),
        producer,
    )
    .await
    .map_err(|_| install_journal_error_response())?;
    if !store.mark_initialized(&install_id).await {
        return Err(install_journal_error_response());
    }

    let failure_memory = state.failure_memory().clone();
    let telemetry = state.telemetry().clone();
    let mc_dir = PathBuf::from(mc_dir);
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_failure_memory = failure_memory.clone();
    let worker_operation_id = operation_id_task.clone();
    let worker_telemetry = telemetry.clone();
    let worker_state = state.clone();
    let worker_runtime_cache = state.managed_runtime_cache().clone();
    let progress_owner = producer.claim_child();
    InstallStore::spawn_tracked_worker_with_interrupt_handler_owned(
        store,
        producer.claim_child(),
        install_id_task,
        interrupted_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let terminal_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let mut coalescer = InstallProgressCoalescer::default();
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        for progress in coalescer.push(progress) {
                            if !record_and_emit_install_progress(
                                store.as_ref(),
                                journals.as_ref(),
                                &operation_id,
                                &install_id,
                                progress,
                                &mut last_journal_phase,
                            )
                            .await
                            {
                                journal_failed.notify_one();
                                return false;
                            }
                        }
                    }
                    if let Some(progress) = coalescer.flush()
                        && !record_and_emit_install_progress(
                            store.as_ref(),
                            journals.as_ref(),
                            &operation_id,
                            &install_id,
                            progress,
                            &mut last_journal_phase,
                        )
                        .await
                    {
                        journal_failed.notify_one();
                        return false;
                    }
                    true
                })
            };

            let installed_library_root = mc_dir.clone();
            let downloader = Downloader::new(mc_dir, worker_runtime_cache);
            let mut repair_resume = InstallRepairResume::default();
            let (final_install_succeeded, final_terminal_progress) = loop {
                if let Ok(mut terminal_progress) = terminal_progress.lock() {
                    *terminal_progress = None;
                }
                let progress_tx_for_downloader = progress_tx.clone();
                let terminal_progress_for_downloader = Arc::clone(&terminal_progress);
                let mut install_facts = Vec::new();
                let mut install_descriptors = Vec::new();
                let install_result = {
                    let install = downloader.install_version_with_facts_and_descriptors(
                        &version_id,
                        move |progress| {
                            if progress.done {
                                if let Ok(mut terminal_progress) =
                                    terminal_progress_for_downloader.lock()
                                {
                                    *terminal_progress = Some(progress);
                                }
                                return;
                            }
                            let _ = progress_tx_for_downloader.send(progress);
                        },
                        |fact| install_facts.push(fact),
                        |descriptor| install_descriptors.push(descriptor),
                    );
                    tokio::pin!(install);
                    tokio::select! {
                        result = &mut install => Some(result),
                        () = journal_failed.notified() => None,
                    }
                };
                let Some(install_result) = install_result else {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(
                        store_task,
                        worker_store.as_ref(),
                        &worker_install_id,
                    )
                    .await;
                    return;
                };
                let attempt_terminal_progress = terminal_progress
                    .lock()
                    .ok()
                    .and_then(|mut progress| progress.take());
                let install_error = match install_result {
                    Ok(receipt) => {
                        match worker_state
                            .accept_known_good_install_receipt(&installed_library_root, receipt)
                            .await
                        {
                            Ok(()) => break (true, attempt_terminal_progress),
                            Err(error) => {
                                tracing::warn!(
                                    operation_id = worker_operation_id.as_str(),
                                    version_id = version_id.as_str(),
                                    failure_kind = "known_good_reconciliation",
                                    "install worker could not accept verified install authority"
                                );
                                let error = known_good_acceptance_download_error(error);
                                break (
                                    false,
                                    Some(install_progress_with_terminal_error(
                                        terminal_failure_progress_or_default(
                                            attempt_terminal_progress,
                                        ),
                                        &error,
                                    )),
                                );
                            }
                        }
                    }
                    Err(error) => error,
                };
                tracing::warn!(
                    operation_id = worker_operation_id.as_str(),
                    version_id = version_id.as_str(),
                    failure_kind = install_error_log_kind(&install_error),
                    "install worker observed failed install"
                );
                let observed_at = chrono::Utc::now().to_rfc3339();
                let repair_outcome = record_install_failure_outcome_and_repair_for_error(
                    worker_journals.as_ref(),
                    worker_failure_memory.as_ref(),
                    &worker_operation_id,
                    &install_error,
                    &install_facts,
                    &install_descriptors,
                    &observed_at,
                )
                .await;
                if repair_resume.resume_after(repair_outcome.as_ref()) {
                    continue;
                }
                break (
                    false,
                    Some(install_progress_with_terminal_error(
                        terminal_failure_progress_or_default(attempt_terminal_progress),
                        &install_error,
                    )),
                );
            };
            let terminal_progress = if final_install_succeeded {
                final_terminal_progress.unwrap_or_else(vanilla_install_done_progress)
            } else {
                final_terminal_progress.unwrap_or_else(observed_install_failure_progress)
            };
            let failure_summary = if !final_install_succeeded {
                let sanitized = sanitize_install_progress(terminal_progress.clone());
                Some(
                    sanitized
                        .error
                        .unwrap_or_else(|| INSTALL_FAILURE_MESSAGE.to_string()),
                )
            } else {
                None
            };
            let _ = progress_tx.send(terminal_progress);
            drop(progress_tx);
            let journal_committed =
                finish_install_progress_task(store_task, worker_store.as_ref(), &worker_install_id)
                    .await;
            if journal_committed && let Some(summary) = failure_summary {
                emit_install_failed(worker_telemetry.as_ref(), &summary);
            }
        },
        move |progress| async move {
            if record_install_operation_interrupted(
                journals.as_ref(),
                &operation_id_task,
                &progress,
            )
            .await
            .is_err()
            {
                tracing::warn!("failed to commit interrupted install journal");
                return false;
            }
            true
        },
    );
    reservation.hand_off();

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
        view_model: InstallProgressViewModel::starting(),
    })
}

pub(super) async fn record_install_failure_outcome_and_repair(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    install_facts: &[ExecutionDownloadFact],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    let evidence = install_failure_evidence_from_download_facts(operation_id, install_facts);
    record_install_failure_evidence_and_repair(
        journals,
        failure_memory,
        operation_id,
        &evidence,
        install_descriptors,
        observed_at,
    )
    .await
}

pub(super) async fn record_install_failure_outcome_and_repair_for_error(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    error: &DownloadError,
    install_facts: &[ExecutionDownloadFact],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    let evidence =
        install_failure_evidence_from_download_error_or_facts(operation_id, error, install_facts);
    record_install_failure_evidence_and_repair(
        journals,
        failure_memory,
        operation_id,
        &evidence,
        install_descriptors,
        observed_at,
    )
    .await
}

fn install_error_log_kind(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::FileOperation(_) => "file_operation",
        DownloadError::ResolveManifest(_) => "resolve_manifest",
        DownloadError::Request(_) => "request",
        DownloadError::ParseVersion(_) => "parse_version",
        DownloadError::PrepareRuntime(_) => "prepare_runtime",
        DownloadError::RuntimeUnavailableForPlatform { .. } => "runtime_unavailable_for_platform",
        DownloadError::RuntimeRosettaRequired { .. } => "runtime_rosetta_required",
        DownloadError::Integrity(_) => "integrity",
        DownloadError::LibraryPlan(_) => "library_plan",
    }
}

async fn record_install_failure_evidence_and_repair(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    if record_install_operation_guardian_evidence(
        journals,
        operation_id,
        evidence,
        crate::state::contracts::OperationPhase::Downloading,
    )
    .await
    .is_err()
    {
        tracing::warn!("failed to commit install Guardian evidence");
        return None;
    }
    let assessment = assess_install_guardian_failure(
        Some(failure_memory),
        operation_id,
        evidence,
        crate::state::contracts::OperationPhase::Downloading,
        observed_at,
    )?;
    if record_install_guardian_terminal_outcome(
        journals,
        Some(failure_memory),
        operation_id,
        evidence,
        crate::state::contracts::OperationPhase::Downloading,
        &assessment,
        observed_at,
    )
    .await
    .is_err()
    {
        tracing::warn!("failed to commit install Guardian outcome");
        return None;
    }
    let repair_client = reqwest::Client::new();
    let repair_outcome = match repair_install_artifact_corruption_with_guardian(
        journals,
        failure_memory,
        &repair_client,
        assessment,
        evidence,
        install_descriptors,
        observed_at,
    )
    .await
    {
        Ok(outcome) => outcome,
        Err(_) => {
            tracing::warn!("failed to commit install artifact-repair journal");
            return None;
        }
    };
    if let Some(repair_outcome) = repair_outcome.as_ref()
        && record_install_operation_guardian_repair_outcome(journals, operation_id, repair_outcome)
            .await
            .is_err()
    {
        tracing::warn!("failed to commit install Guardian repair outcome");
        return None;
    }
    repair_outcome
}

fn terminal_failure_progress_or_default(progress: Option<DownloadProgress>) -> DownloadProgress {
    progress
        .filter(|progress| progress.error.is_some())
        .unwrap_or_else(observed_install_failure_progress)
}

fn known_good_acceptance_download_error(error: std::io::Error) -> DownloadError {
    DownloadError::FileOperation(std::io::Error::new(
        error.kind(),
        "verified install authority could not be reconciled",
    ))
}

fn emit_install_failed(telemetry: &TelemetryHub, summary: &str) {
    telemetry.emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::InstallFailed,
        TelemetryErrorArea::Install,
        TelemetryErrorLevel::Error,
        summary,
    ));
}

async fn record_and_emit_install_progress(
    store: &InstallStore,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    progress: DownloadProgress,
    last_journal_phase: &mut Option<String>,
) -> bool {
    if record_install_operation_progress(journals, operation_id, &progress, last_journal_phase)
        .await
        .is_err()
    {
        tracing::warn!("failed to record install journal progress");
        return false;
    }
    store
        .emit_record(install_id, install_progress_record(progress))
        .await;
    true
}

fn install_journal_error_response() -> InstallApplicationError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "Could not start the install safely. Check app data permissions and try again."
        })),
    )
}

async fn finish_install_progress_task(
    task: tokio::task::JoinHandle<bool>,
    store: &InstallStore,
    install_id: &str,
) -> bool {
    match task.await {
        Ok(true) => true,
        Ok(false) => {
            store.remove(install_id).await;
            false
        }
        Err(error) if error.is_panic() => std::panic::resume_unwind(error.into_panic()),
        Err(error) => panic!("install progress task stopped unexpectedly: {error}"),
    }
}

fn vanilla_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub async fn install_status(
    state: &AppState,
    id: &str,
) -> Result<InstallStatusResponse, InstallApplicationError> {
    let operation_id = install_operation_id(id);
    let snapshot = state.installs().snapshot(id).await;
    let journal = state.journals().get(&operation_id);
    if snapshot.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        ));
    }

    let done = snapshot.as_ref().is_some_and(|snapshot| snapshot.done)
        || journal
            .as_ref()
            .is_some_and(|journal| install_journal_is_terminal(journal.status));
    let progress = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.latest.as_ref())
        .map(|record| vec![record.progress.clone()])
        .or_else(|| journal.as_ref().map(install_progress_history_from_journal))
        .unwrap_or_default()
        .into_iter()
        .map(sanitize_install_progress)
        .collect::<Vec<_>>();
    let view_model = progress
        .last()
        .map(vanilla_install_progress_view_model)
        .unwrap_or_else(InstallProgressViewModel::starting);
    let failure_point = journal
        .as_ref()
        .and_then(install_failure_point_from_journal);
    let guardian_repair = journal
        .as_ref()
        .and_then(install_guardian_repair_summary_from_journal);
    let guardian = journal
        .as_ref()
        .and_then(install_guardian_outcome_summary_from_journal);
    let failure_view_model =
        install_failure_view_model(&view_model, guardian.as_ref(), guardian_repair.as_ref());
    let proof = journal
        .as_ref()
        .filter(|journal| install_journal_is_terminal(journal.status))
        .map(operation_journal_proof_record);

    Ok(InstallStatusResponse {
        install_id: public_install_id(id),
        operation_id,
        done,
        progress,
        view_model,
        failure_view_model,
        failure_point,
        guardian,
        guardian_repair,
        proof,
    })
}

pub(crate) async fn install_queue_status_owned(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let started = maybe_start_next_queued_install(state, handoff).await?;
    Ok(install_queue_state_response(state, None, started).await)
}

pub(crate) async fn enqueue_install_owned(
    state: &AppState,
    request: InstallQueueRequest,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(state, request, InstallQueuePlacement::Back, handoff).await
}

pub(crate) async fn enqueue_install_from_continuation(
    state: &AppState,
    request: InstallQueueRequest,
    producer: ProducerLease,
) -> Result<ContinuationInstallQueueResult, InstallApplicationError> {
    let selection = enqueue_install_request(state, request, InstallQueuePlacement::Back).await?;
    let selected_queue_id = install_queue_outcome_id(&selection.outcome).to_string();
    let owns_selected_queue = matches!(
        &selection.outcome,
        InstallQueueEnqueueOutcome::Enqueued { .. }
    );
    let started = if matches!(
        &selection.outcome,
        InstallQueueEnqueueOutcome::AlreadyActive { .. }
    ) {
        None
    } else {
        let start_state = state.clone();
        maybe_start_selected_queued_install_owned_with(
            state,
            &selected_queue_id,
            owns_selected_queue,
            |spec| {
                let state = start_state.clone();
                let attempt_owner = producer.claim_child();
                async move { start_queued_install(&state, &spec, &attempt_owner).await }
            },
        )
        .await?
    };
    if let Some(started) = started.as_ref() {
        spawn_install_queue_monitor_owned(state.clone(), started.install_id.clone(), producer);
    }
    let response =
        install_queue_state_response(state, Some(selection.notice), started.clone()).await;
    Ok(ContinuationInstallQueueResult {
        response,
        outcome: selection.outcome,
    })
}

pub(crate) async fn retry_install_owned(
    state: &AppState,
    request: InstallQueueRequest,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(state, request, InstallQueuePlacement::Front, handoff).await
}

pub async fn remove_queued_install(
    state: &AppState,
    queue_id: &str,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let removed = state.installs().remove_queued_install(queue_id).await;
    let notice = removed
        .as_ref()
        .map(|entry| {
            install_queue_notice(
                "removed",
                "info",
                "Removed from queue",
                Some(install_queue_label(&entry.spec)),
            )
        })
        .or_else(|| {
            Some(install_queue_notice(
                "remove_unavailable",
                "warn",
                "Queued install was not removed",
                Some("It may have already started or left the queue.".to_string()),
            ))
        });
    Ok(install_queue_state_response(state, notice, None).await)
}

async fn enqueue_install_with_placement(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let selection = enqueue_install_request(state, request, placement).await?;
    let started = maybe_start_next_queued_install(state, handoff).await?;
    Ok(install_queue_state_response(state, Some(selection.notice), started).await)
}

async fn enqueue_install_request(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
) -> Result<InstallQueueSelection, InstallApplicationError> {
    let spec = install_queue_spec_from_request(state, request).await?;
    let queue_id = generate_install_id("install-queue");
    let outcome = state
        .installs()
        .enqueue_queued_install(queue_id, spec.clone(), placement)
        .await;
    let notice = install_queue_notice_for_outcome(&outcome, &spec, placement);
    Ok(InstallQueueSelection { notice, outcome })
}

async fn install_queue_spec_from_request(
    state: &AppState,
    request: InstallQueueRequest,
) -> Result<InstallQueueSpec, InstallApplicationError> {
    match request {
        InstallQueueRequest::Vanilla { version_id } => {
            let version_id = version_id.trim().to_string();
            if version_id.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "version_id is required" })),
                ));
            }
            state.library_dir().ok_or_else(|| {
                (
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({ "error": "Axial library is not configured" })),
                )
            })?;
            Ok(InstallQueueSpec::vanilla(version_id))
        }
        InstallQueueRequest::Loader {
            component_id,
            build_id,
        } => {
            let build_id = build_id.trim().to_string();
            if build_id.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "build_id is required" })),
                ));
            }
            state.library_dir().ok_or_else(|| {
                (
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({ "error": "Axial library is not configured" })),
                )
            })?;
            let build = resolve_build_record_for_install(component_id, &build_id)
                .await
                .map_err(loader_pre_operation_error_response)?;
            Ok(InstallQueueSpec::loader(
                build.component_id,
                build.build_id,
                build.version_id,
                build.minecraft_version,
                build.loader_version,
            ))
        }
    }
}

async fn maybe_start_next_queued_install(
    state: &AppState,
    handoff: RequestProducerHandoff,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    let producer = handoff
        .try_claim()
        .map_err(|_| install_shutdown_error_response())?;
    let transaction_owner = producer.claim_child();
    let transaction_state = state.clone();
    let transaction = transaction_owner.spawn_joinable(async move {
        let _queue_start = transaction_state
            .installs()
            .acquire_queue_start_gate()
            .await;
        let Some(entry) = transaction_state
            .installs()
            .reserve_next_queued_install()
            .await
        else {
            return Ok(None);
        };
        let started = match start_queued_install(&transaction_state, &entry.spec, &producer).await {
            Ok(started) => started,
            Err(error) => {
                transaction_state
                    .installs()
                    .discard_active_queued_install(&entry.queue_id)
                    .await;
                return Err(error);
            }
        };
        if !transaction_state
            .installs()
            .mark_queued_install_started(&entry.queue_id, started.install_id.clone())
            .await
        {
            return Err(install_queue_start_stopped_error_response());
        }
        spawn_install_queue_monitor_owned(
            transaction_state.clone(),
            started.install_id.clone(),
            producer,
        );
        Ok(Some(started))
    });
    transaction
        .await
        .map_err(|_| install_queue_start_stopped_error_response())?
}

async fn maybe_start_next_queued_install_owned(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    let _queue_start = state.installs().acquire_queue_start_gate().await;
    let Some(entry) = state.installs().reserve_next_queued_install().await else {
        return Ok(None);
    };
    let started = match start_queued_install(state, &entry.spec, producer).await {
        Ok(started) => started,
        Err(error) => {
            state
                .installs()
                .discard_active_queued_install(&entry.queue_id)
                .await;
            return Err(error);
        }
    };
    if !state
        .installs()
        .mark_queued_install_started(&entry.queue_id, started.install_id.clone())
        .await
    {
        return Err(install_queue_start_stopped_error_response());
    }
    Ok(Some(started))
}

async fn maybe_start_selected_queued_install_owned_with<Start, StartFuture>(
    state: &AppState,
    selected_queue_id: &str,
    owns_selected_queue: bool,
    mut start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnMut(InstallQueueSpec) -> StartFuture,
    StartFuture: Future<Output = Result<InstallStartResponse, InstallApplicationError>>,
{
    let _queue_start = state.installs().acquire_queue_start_gate().await;
    let initial_pending = state.installs().queue_snapshot().await.pending.len();
    for _ in 0..initial_pending.saturating_add(1) {
        let Some(entry) = state.installs().reserve_next_queued_install().await else {
            return selected_queue_residual(state, selected_queue_id, owns_selected_queue).await;
        };
        match start(entry.spec.clone()).await {
            Ok(started) => {
                if !state
                    .installs()
                    .mark_queued_install_started(&entry.queue_id, started.install_id.clone())
                    .await
                {
                    return Err(selected_queue_missing_error_response());
                }
                return Ok(Some(started));
            }
            Err(error) => {
                state
                    .installs()
                    .discard_active_queued_install(&entry.queue_id)
                    .await;
                if entry.queue_id == selected_queue_id {
                    return Err(error);
                }
            }
        }
    }
    selected_queue_residual(state, selected_queue_id, owns_selected_queue).await
}

async fn selected_queue_residual(
    state: &AppState,
    selected_queue_id: &str,
    owns_selected_queue: bool,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    let snapshot = state.installs().queue_snapshot().await;
    let selected_is_committed_active = snapshot
        .active
        .as_ref()
        .is_some_and(|active| active.queue_id == selected_queue_id && active.install_id.is_some());
    let selected_is_pending = snapshot
        .pending
        .iter()
        .any(|entry| entry.queue_id == selected_queue_id);
    let committed_active_owner = snapshot
        .active
        .as_ref()
        .is_some_and(|active| active.install_id.is_some());
    if selected_is_committed_active || (selected_is_pending && committed_active_owner) {
        Ok(None)
    } else {
        if owns_selected_queue {
            if snapshot.active.as_ref().is_some_and(|active| {
                active.queue_id == selected_queue_id && active.install_id.is_none()
            }) {
                state
                    .installs()
                    .discard_active_queued_install(selected_queue_id)
                    .await;
            } else {
                state
                    .installs()
                    .remove_queued_install(selected_queue_id)
                    .await;
            }
        }
        Err(selected_queue_missing_error_response())
    }
}

async fn start_queued_install(
    state: &AppState,
    spec: &InstallQueueSpec,
    producer: &ProducerLease,
) -> Result<InstallStartResponse, InstallApplicationError> {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => {
            start_install_version_owned(
                state,
                InstallVersionStartRequest {
                    version_id: version_id.clone(),
                },
                producer,
            )
            .await
        }
        InstallQueueSpec::Loader {
            component_id,
            build_id,
            ..
        } => {
            start_loader_install_owned(
                state,
                LoaderInstallStartRequest {
                    component_id: *component_id,
                    build_id: build_id.clone(),
                },
                producer,
            )
            .await
        }
    }
}

fn spawn_install_queue_monitor_owned(state: AppState, install_id: String, producer: ProducerLease) {
    let successor_owner = producer.claim_child();
    producer.spawn(async move {
        let mut install_id = install_id;
        let mut shutdown = state.subscribe_shutdown();
        loop {
            wait_for_install_terminal(&state, &install_id).await;
            state.invalidate_installed_versions();
            state
                .installs()
                .clear_active_queued_install(&install_id)
                .await;
            if *shutdown.borrow_and_update() {
                return;
            }
            let Ok(successor) = successor_owner.try_claim_successor() else {
                return;
            };
            let Ok(Some(started_install)) =
                maybe_start_next_queued_install_owned(&state, &successor).await
            else {
                return;
            };
            install_id = started_install.install_id;
        }
    });
}

#[cfg(test)]
fn spawn_install_queue_monitor(state: AppState, install_id: String) {
    let producer = state
        .try_claim_producer()
        .expect("claim test install monitor");
    spawn_install_queue_monitor_owned(state, install_id, producer);
}

fn install_shutdown_error_response() -> InstallApplicationError {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({
            "error": "Installs are unavailable while the application is shutting down."
        })),
    )
}

fn install_queue_start_stopped_error_response() -> InstallApplicationError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "The queued install stopped before startup settled. Try again."
        })),
    )
}

fn selected_queue_missing_error_response() -> InstallApplicationError {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "The selected install left the queue before it could start. Try again."
        })),
    )
}

async fn wait_for_install_terminal(state: &AppState, install_id: &str) {
    let Some((snapshot, mut receiver)) = state.installs().subscribe_records(install_id).await
    else {
        return;
    };
    if snapshot.done
        || snapshot
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.done)
    {
        return;
    }
    loop {
        match receiver.recv().await {
            Ok(record) if record.progress.done => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
        }
    }
}

async fn install_queue_state_response(
    state: &AppState,
    notice: Option<InstallQueueNoticeViewModel>,
    started_install: Option<InstallStartResponse>,
) -> InstallQueueStateResponse {
    let snapshot = state.installs().queue_snapshot().await;
    let active = install_queue_active_view_model(state, snapshot.active.as_ref()).await;
    let items = install_queue_item_view_models(&snapshot);
    let view_model = install_queue_view_model(active.as_ref(), &items);
    InstallQueueStateResponse {
        active,
        items,
        view_model,
        notice,
        started_install,
    }
}

async fn install_queue_active_view_model(
    state: &AppState,
    active: Option<&ActiveQueuedInstallEntry>,
) -> Option<InstallQueueActiveViewModel> {
    let active = active?;
    let install_id = active.install_id.clone();
    let progress = match install_id.as_deref() {
        Some(install_id) => {
            install_queue_active_progress_view_model(state, install_id, &active.spec).await
        }
        None => InstallProgressViewModel::starting(),
    };
    let label = install_queue_label(&active.spec);
    let title = if install_id.is_some() {
        "Installing"
    } else {
        "Starting install"
    };
    let summary = if install_id.is_some() {
        format!("{label} is installing.")
    } else {
        format!("{label} is starting.")
    };
    Some(InstallQueueActiveViewModel {
        queue_id: active.queue_id.clone(),
        operation_id: install_id
            .as_ref()
            .map(|install_id| install_operation_id(install_id)),
        install_id,
        install_started_at_ms: active.install_started_at_ms,
        kind: install_queue_kind(&active.spec).to_string(),
        title: title.to_string(),
        summary,
        label,
        install_item: install_queue_install_item(&active.spec),
        progress,
    })
}

async fn install_queue_active_progress_view_model(
    state: &AppState,
    install_id: &str,
    spec: &InstallQueueSpec,
) -> InstallProgressViewModel {
    let snapshot = state.installs().snapshot(install_id).await;
    let progress = snapshot.and_then(|snapshot| snapshot.latest.map(|record| record.progress));
    let Some(progress) = progress else {
        return InstallProgressViewModel::starting();
    };
    if spec.is_loader() {
        loader_install_progress_view_model(&progress)
    } else {
        vanilla_install_progress_view_model(&progress)
    }
}

fn install_queue_item_view_models(
    snapshot: &InstallQueueSnapshot,
) -> Vec<InstallQueuedItemViewModel> {
    let total = snapshot.pending.len();
    snapshot
        .pending
        .iter()
        .enumerate()
        .map(|(index, entry)| install_queue_item_view_model(entry, index + 1, total))
        .collect()
}

fn install_queue_item_view_model(
    entry: &QueuedInstallEntry,
    position: usize,
    total: usize,
) -> InstallQueuedItemViewModel {
    let label = install_queue_label(&entry.spec);
    InstallQueuedItemViewModel {
        queue_id: entry.queue_id.clone(),
        state_id: "queued".to_string(),
        kind: install_queue_kind(&entry.spec).to_string(),
        title: "Install queued".to_string(),
        summary: if position == 1 {
            format!("{label} is next to start.")
        } else {
            format!("{label} is waiting for earlier downloads.")
        },
        detail: if position == 1 {
            format!("Position 1 of {total}; next to start when the download slot opens.")
        } else {
            let waiting = position.saturating_sub(1);
            format!(
                "Position {position} of {total}; waiting behind {waiting} {}.",
                if waiting == 1 { "item" } else { "items" }
            )
        },
        label,
        position,
        total,
        install_item: install_queue_install_item(&entry.spec),
        remove_action: InstallActionViewModel {
            action: "remove_from_queue".to_string(),
            label: "Remove from queue".to_string(),
            enabled: true,
            disabled_reason: None,
        },
    }
}

fn install_queue_view_model(
    active: Option<&InstallQueueActiveViewModel>,
    items: &[InstallQueuedItemViewModel],
) -> InstallQueueViewModel {
    let queued_count = items.len();
    let queued_count_label = match queued_count {
        0 => "No queued downloads".to_string(),
        1 => "1 queued".to_string(),
        count => format!("{count} queued"),
    };
    let queued_item_label = match queued_count {
        0 => "No items queued".to_string(),
        1 => "1 item queued".to_string(),
        count => format!("{count} items queued"),
    };
    let next_label = items.first().map(|item| item.label.clone());
    let state_id = if active.is_some() {
        "active"
    } else if queued_count > 0 {
        "queued"
    } else {
        "idle"
    };
    let title = if active.is_some() {
        "Downloads active".to_string()
    } else if queued_count > 0 {
        "Downloads queued".to_string()
    } else {
        "Nothing downloading".to_string()
    };
    let summary = if active.is_some() {
        if queued_count > 0 {
            format!("{queued_item_label} behind the active install.")
        } else {
            "No queued downloads behind the active install.".to_string()
        }
    } else if queued_count > 0 {
        format!("{queued_item_label} and waiting to start. The next item will begin automatically.")
    } else {
        "Launch an instance that needs a download, or install a new Minecraft version, and it will show up here."
            .to_string()
    };
    let active_queued_count_label = (queued_count > 0).then(|| format!(" · {queued_count_label}"));
    InstallQueueViewModel {
        state_id: state_id.to_string(),
        status_label: if active.is_some() {
            "Installing".to_string()
        } else if queued_count > 0 {
            "Queued".to_string()
        } else {
            "Idle".to_string()
        },
        title,
        summary,
        queued_count,
        queued_count_label,
        queued_item_label,
        next_label,
        active_queued_count_label,
        section_title: "Queue".to_string(),
        empty_title: "Nothing downloading".to_string(),
        empty_summary:
            "Launch an instance that needs a download, or install a new Minecraft version, and it will show up here."
                .to_string(),
    }
}

fn install_queue_notice_for_outcome(
    outcome: &InstallQueueEnqueueOutcome,
    spec: &InstallQueueSpec,
    placement: InstallQueuePlacement,
) -> InstallQueueNoticeViewModel {
    let label = install_queue_label(spec);
    match outcome {
        InstallQueueEnqueueOutcome::Enqueued { .. } => {
            if placement == InstallQueuePlacement::Front {
                install_queue_notice("retry_queued", "info", "Retry queued", Some(label))
            } else {
                install_queue_notice("queued", "info", "Install queued", Some(label))
            }
        }
        InstallQueueEnqueueOutcome::AlreadyActive { .. } => install_queue_notice(
            "already_active",
            "info",
            "Install already active",
            Some(label),
        ),
        InstallQueueEnqueueOutcome::AlreadyQueued { .. } => install_queue_notice(
            "already_queued",
            "info",
            "Install already queued",
            Some(label),
        ),
        InstallQueueEnqueueOutcome::MovedToFront { .. } => install_queue_notice(
            "retry_moved_next",
            "info",
            "Retry moved to the front of the queue",
            Some(label),
        ),
    }
}

fn install_queue_outcome_id(outcome: &InstallQueueEnqueueOutcome) -> &str {
    match outcome {
        InstallQueueEnqueueOutcome::Enqueued { queue_id }
        | InstallQueueEnqueueOutcome::AlreadyActive { queue_id }
        | InstallQueueEnqueueOutcome::AlreadyQueued { queue_id }
        | InstallQueueEnqueueOutcome::MovedToFront { queue_id } => queue_id,
    }
}

fn install_queue_notice(
    state_id: &str,
    tone: &str,
    message: &str,
    detail: Option<String>,
) -> InstallQueueNoticeViewModel {
    InstallQueueNoticeViewModel {
        state_id: state_id.to_string(),
        tone: tone.to_string(),
        message: message.to_string(),
        detail,
    }
}

fn install_queue_kind(spec: &InstallQueueSpec) -> &'static str {
    match spec {
        InstallQueueSpec::Vanilla { .. } => "vanilla",
        InstallQueueSpec::Loader { .. } => "loader",
    }
}

fn install_queue_label(spec: &InstallQueueSpec) -> String {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => {
            if version_id.trim().is_empty() {
                "Minecraft".to_string()
            } else {
                format!("Minecraft {}", version_id.trim())
            }
        }
        InstallQueueSpec::Loader {
            component_id,
            minecraft_version,
            loader_version,
            ..
        } => {
            let loader_name = component_id.display_name();
            let label = if loader_version.trim().is_empty() {
                format!("{loader_name} loader")
            } else {
                format!("{loader_name} {}", loader_version.trim())
            };
            if minecraft_version.trim().is_empty() {
                label
            } else {
                format!("{label} for Minecraft {}", minecraft_version.trim())
            }
        }
    }
}

fn install_queue_install_item(spec: &InstallQueueSpec) -> InstallQueueInstallItemViewModel {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => InstallQueueInstallItemViewModel {
            version_id: version_id.clone(),
            loader: None,
        },
        InstallQueueSpec::Loader {
            component_id,
            build_id,
            target_version_id,
            minecraft_version,
            loader_version,
        } => InstallQueueInstallItemViewModel {
            version_id: target_version_id.clone(),
            loader: Some(InstallQueueLoaderItemViewModel {
                component_id: component_id.as_str().to_string(),
                build_id: build_id.clone(),
                minecraft_version: minecraft_version.clone(),
                loader_version: loader_version.clone(),
            }),
        },
    }
}

pub(crate) fn effective_install_version_id(request: &InstallVersionStartRequest) -> String {
    request.version_id.trim().to_string()
}

fn install_failure_view_model(
    progress: &InstallProgressViewModel,
    guardian: Option<&GuardianInstallOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> Option<InstallFailureViewModel> {
    if !progress.failed {
        return None;
    }

    let summary = guardian
        .map(|guardian| guardian.label().to_string())
        .or_else(|| repair.map(|repair| repair.label.clone()))
        .unwrap_or_else(|| progress.label.clone());
    let mut details = Vec::new();
    push_install_failure_detail(
        &mut details,
        guardian.and_then(|guardian| guardian.detail().map(str::to_string)),
    );
    if let Some(guardian) = guardian {
        for guidance in guardian.guidance() {
            push_install_failure_detail(&mut details, Some(guidance.clone()));
        }
    }
    push_install_failure_detail(&mut details, repair.map(|repair| repair.label.clone()));
    push_install_failure_detail(
        &mut details,
        repair.and_then(|repair| repair.detail.clone()),
    );

    Some(InstallFailureViewModel {
        state_id: failure_state_id(guardian, repair).to_string(),
        title: "Install failed".to_string(),
        tone: "err".to_string(),
        detail: details.first().cloned(),
        details,
        retry_action: install_retry_action(guardian, repair),
        dismiss_action: InstallActionViewModel {
            action: "dismiss".to_string(),
            label: "Dismiss".to_string(),
            enabled: true,
            disabled_reason: None,
        },
        repair_action: install_repair_action(repair),
        summary,
    })
}

fn push_install_failure_detail(details: &mut Vec<String>, detail: Option<String>) {
    let Some(detail) = detail.map(|detail| detail.trim().to_string()) else {
        return;
    };
    if detail.is_empty() || details.iter().any(|existing| existing == &detail) {
        return;
    }
    details.push(detail);
}

fn failure_state_id(
    guardian: Option<&GuardianInstallOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> &'static str {
    if let Some(repair) = repair {
        return match repair.status.as_str() {
            "repaired" => "failed_repair_applied",
            "suppressed" => "failed_repair_suppressed",
            "blocked" => "failed_repair_blocked",
            "failed" => "failed_repair_failed",
            _ => "failed_repair_recorded",
        };
    }
    match guardian.map(GuardianInstallOutcomeSummary::decision) {
        Some("retry") => "failed_retryable",
        Some("block") => "failed_blocked",
        Some("suppress") => "failed_suppressed",
        Some(_) => "failed_guardian_recorded",
        None => "failed",
    }
}

fn install_retry_action(
    guardian: Option<&GuardianInstallOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> InstallActionViewModel {
    if repair.is_some_and(|repair| repair.status == "repaired") {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: true,
            disabled_reason: None,
        };
    }

    if let Some(guardian) = guardian.filter(|guardian| {
        guardian.decision() == "block" && !blocking_guardian_allows_retry(guardian)
    }) {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: false,
            disabled_reason: Some(guardian.retry_disabled_reason().to_string()),
        };
    }

    InstallActionViewModel {
        action: "retry".to_string(),
        label: "Retry install".to_string(),
        enabled: true,
        disabled_reason: None,
    }
}

fn blocking_guardian_allows_retry(guardian: &GuardianInstallOutcomeSummary) -> bool {
    guardian.diagnosis_id() == DiagnosisId::ManagedRuntimeRosettaRequired
}

fn install_repair_action(repair: Option<&InstallGuardianRepairSummary>) -> InstallActionViewModel {
    let Some(repair) = repair else {
        return InstallActionViewModel {
            action: "repair".to_string(),
            label: "Automatic repair unavailable".to_string(),
            enabled: false,
            disabled_reason: Some("No automatic repair is available for this failure.".to_string()),
        };
    };

    let label = match repair.status.as_str() {
        "repaired" => "Automatic repair applied",
        "blocked" => "Automatic repair blocked",
        "failed" => "Automatic repair failed",
        "suppressed" => "Automatic repair paused",
        _ => "Automatic repair recorded",
    };
    InstallActionViewModel {
        action: "repair".to_string(),
        label: label.to_string(),
        enabled: false,
        disabled_reason: repair.detail.clone().or_else(|| Some(repair.label.clone())),
    }
}

fn generate_install_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{:032x}", nanos)
}

#[cfg(test)]
mod tests;
