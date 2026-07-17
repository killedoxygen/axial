//! Application-owned install orchestration facade.
//!
//! The facade owns request/response contracts, vanilla install worker
//! coordination, and status composition. Child modules own loader workflows,
//! operation journal/progress mapping, Guardian failure mapping, and event
//! streaming. Core Minecraft code still owns provider resolution, download
//! verification, and concrete install effects.

mod loader;
mod model;
mod operation;
mod stream;

#[cfg(test)]
pub(crate) use operation::loader_install_guardian_evidence_kind;

use super::InstallVersionCommand;
use crate::application::instances::instance_version_is_installed_and_launchable;
use crate::guardian::{
    DiagnosisId, GuardianInstallArtifactFailureEvidence, GuardianInstallOutcomeSummary,
};
use crate::observability::{
    operation_journal_proof_record,
    telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
    },
};
use crate::state::AppState;
use crate::state::contracts::{OperationId, OperationJournalEntry, OperationPhase};
use crate::state::{
    ActiveQueuedInstallEntry, ContentQueueAction, InstallInitializationStatus,
    InstallQueueEnqueueOutcome, InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec,
    InstallStore, IntegrityForegroundLease, IntegrityForegroundRegistration,
    OperationJournalReconciliation, OperationJournalStore, OperationJournalStoreError,
    ProducerLease, QueuedContentSelection, QueuedInstallEntry, RequestProducerHandoff,
    SetupInstanceBaseline, SetupInstanceCleanup, SetupInstancePathKind, SetupInstancePathSnapshot,
    UpdateOperationAdmissionError, UpdateOperationLease, operation_journal_plan_is_visible,
};
use axial_config::{INSTANCE_LAYOUT_DIRS, Instance, SHARED_INSTANCE_FILES};
use axial_minecraft::{
    DownloadError, DownloadProgress, Downloader, download::ExecutionDownloadFact,
    resolve_build_record_for_install,
};
use axum::{Json, http::StatusCode};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
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
const CONTENT_INSTANCE_REMOVED_PHASE: &str = "error_instance_removed";

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
    foreground: Option<IntegrityForegroundLease>,
}

struct ContentInitializationReservation {
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: Option<String>,
    operation_id: OperationId,
    expected_plan: OperationJournalEntry,
    cleanup_owner: Option<ProducerLease>,
}

impl ContentInitializationReservation {
    fn new(
        store: Arc<InstallStore>,
        journals: Arc<OperationJournalStore>,
        install_id: String,
        operation_id: OperationId,
        expected_plan: OperationJournalEntry,
        cleanup_owner: ProducerLease,
    ) -> Self {
        Self {
            store,
            journals,
            install_id: Some(install_id),
            operation_id,
            expected_plan,
            cleanup_owner: Some(cleanup_owner),
        }
    }

    fn hand_off(mut self) {
        self.install_id = None;
    }
}

impl Drop for ContentInitializationReservation {
    fn drop(&mut self) {
        let Some(install_id) = self.install_id.take() else {
            return;
        };
        let store = self.store.clone();
        let journals = self.journals.clone();
        let operation_id = self.operation_id.clone();
        let expected_plan = self.expected_plan.clone();
        let cleanup_owner = self
            .cleanup_owner
            .take()
            .expect("content initialization cleanup owner remains available");
        cleanup_owner.spawn(async move {
            settle_content_initialization_cleanup(&journals, &operation_id, &expected_plan).await;
            store.remove(&install_id).await;
        });
    }
}

impl InstallInitializationReservation {
    fn new(
        store: Arc<InstallStore>,
        journals: Arc<OperationJournalStore>,
        install_id: String,
        operation_id: OperationId,
        cleanup_owner: ProducerLease,
        foreground: IntegrityForegroundLease,
    ) -> Self {
        Self {
            store,
            journals,
            install_id: Some(install_id),
            operation_id,
            cleanup_owner: Some(cleanup_owner),
            foreground: Some(foreground),
        }
    }

    fn hand_off(mut self) -> IntegrityForegroundLease {
        self.install_id = None;
        self.foreground
            .take()
            .expect("install initialization foreground owner remains available")
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
        let foreground = self
            .foreground
            .take()
            .expect("install initialization foreground owner remains available");
        cleanup_owner.spawn(async move {
            let _foreground = foreground;
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
    foreground: IntegrityForegroundLease,
) -> Result<InstallInitializationReservation, ()> {
    let reservation = InstallInitializationReservation::new(
        store,
        journals.clone(),
        install_id,
        operation_id.clone(),
        producer.claim_child(),
        foreground,
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

async fn begin_content_journal_with_owned_reconciliation(
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: String,
    operation_id: OperationId,
    instance_id: String,
    producer: &ProducerLease,
) -> Result<ContentInitializationReservation, ()> {
    let expected = operation::planned_content_journal(&operation_id, &instance_id);
    let reservation = ContentInitializationReservation::new(
        store,
        journals.clone(),
        install_id,
        operation_id.clone(),
        expected.clone(),
        producer.claim_child(),
    );
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.claim_child().spawn(async move {
        let mut result =
            begin_content_operation_journal(&journals, &operation_id, &instance_id).await;
        loop {
            match result {
                Ok(()) => {
                    let _ = result_tx.send(Ok(reservation));
                    return;
                }
                Err(error)
                    if matches!(
                        &error,
                        OperationJournalStoreError::Persistence(_)
                            | OperationJournalStoreError::RetryRequired
                    ) =>
                {
                    match reconcile_install_journal_transition(
                        &journals,
                        &operation_id,
                        error,
                        |entry| operation_journal_plan_is_visible(entry, &expected),
                    )
                    .await
                    {
                        Ok(InstallJournalReconciliation::MutationCommitted) => {
                            let _ = result_tx.send(Ok(reservation));
                            return;
                        }
                        Ok(InstallJournalReconciliation::RetryMutation) => {
                            result = begin_content_operation_journal(
                                &journals,
                                &operation_id,
                                &instance_id,
                            )
                            .await;
                        }
                        Err(_) => {
                            let _ = result_tx.send(Err(()));
                            return;
                        }
                    }
                }
                Err(_) => {
                    let _ = result_tx.send(Err(()));
                    return;
                }
            }
        }
    });
    result_rx.await.unwrap_or(Err(()))
}

async fn settle_content_initialization_cleanup(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    expected_plan: &OperationJournalEntry,
) {
    loop {
        match journals.get(operation_id) {
            Some(entry) if operation_journal_plan_is_visible(&entry, expected_plan) => break,
            Some(_) => return,
            None if !journals.has_retry_candidate() => return,
            None => {}
        }
        match reconcile_install_journal_transition(
            journals,
            operation_id,
            OperationJournalStoreError::RetryRequired,
            |entry| operation_journal_plan_is_visible(entry, expected_plan),
        )
        .await
        {
            Ok(InstallJournalReconciliation::MutationCommitted) => break,
            Ok(InstallJournalReconciliation::RetryMutation) | Err(_) => {
                tokio::time::sleep(INSTALL_JOURNAL_RETRY_MAX_DELAY).await;
            }
        }
    }

    loop {
        match operation::record_content_operation_initialization_cancelled(journals, operation_id)
            .await
        {
            Ok(()) => return,
            Err(
                OperationJournalStoreError::Persistence(_)
                | OperationJournalStoreError::RetryRequired,
            ) => {
                tokio::time::sleep(INSTALL_JOURNAL_RETRY_MAX_DELAY).await;
            }
            Err(_) => return,
        }
    }
}

pub type InstallApplicationError = (StatusCode, Json<serde_json::Value>);

#[derive(Clone)]
struct InstallForegroundActivity {
    lease: Arc<Mutex<Option<IntegrityForegroundLease>>>,
    _update_admission: UpdateOperationLease,
}

impl InstallForegroundActivity {
    fn new_with_update_admission(
        lease: IntegrityForegroundLease,
        update_admission: UpdateOperationLease,
    ) -> Self {
        Self {
            lease: Arc::new(Mutex::new(Some(lease))),
            _update_admission: update_admission,
        }
    }

    fn release(&self) {
        drop(
            self.lease
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take(),
        );
    }

    fn retain(&self, lease: IntegrityForegroundLease) {
        let replaced = self
            .lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .replace(lease);
        assert!(
            replaced.is_none(),
            "install foreground activity retained more than one lease"
        );
    }

    fn retained(&self) -> Option<IntegrityForegroundLease> {
        self.lease
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
            .map(IntegrityForegroundLease::retained)
    }
}

fn register_install_foreground(
    state: &AppState,
) -> Result<IntegrityForegroundRegistration, InstallApplicationError> {
    state
        .register_integrity_foreground()
        .map_err(|_| install_shutdown_error_response())
}

async fn retain_install_foreground(
    state: &AppState,
    foreground: &InstallForegroundActivity,
) -> Option<IntegrityForegroundLease> {
    if let Some(retained) = foreground.retained() {
        return Some(retained);
    }
    let Ok(registration) = register_install_foreground(state) else {
        return None;
    };
    let lease = registration.wait_for_settlement().await;
    let retained = lease.retained();
    foreground.retain(lease);
    Some(retained)
}

fn spawn_install_foreground_retention(
    state: AppState,
    install_id: String,
    producer: ProducerLease,
    foreground: InstallForegroundActivity,
) {
    producer.spawn(async move {
        wait_for_install_terminal(&state, &install_id).await;
        drop(foreground);
    });
}

pub(crate) struct ContinuationInstallQueueResult {
    pub response: InstallQueueStateResponse,
    pub outcome: InstallQueueEnqueueOutcome,
}

struct InstallQueueSelection {
    notice: InstallQueueNoticeViewModel,
    outcome: InstallQueueEnqueueOutcome,
}

use loader::start_loader_install_with_foreground;
#[cfg(test)]
use loader::{
    dispatch_loader_install_failure, loader_install_done_progress, loader_install_error_progress,
    observe_active_vanilla_base_install, publish_known_good_loader_terminal,
    require_exact_loader_receipt_version, wait_for_observed_vanilla_base_install,
};
pub use loader::{
    loader_builds, loader_components, loader_game_versions, loader_pre_operation_error_response,
};
pub use model::{
    InstallActionViewModel, InstallFailureViewModel, InstallProgressStepViewModel,
    InstallProgressViewModel, InstallQueueActiveViewModel, InstallQueueContentActionRequest,
    InstallQueueContentItemViewModel, InstallQueueContentSelection,
    InstallQueueInstallItemViewModel, InstallQueueLoaderItemViewModel, InstallQueueNoticeViewModel,
    InstallQueueRequest, InstallQueueStateResponse, InstallQueueViewModel,
    InstallQueuedItemViewModel, InstallStartResponse, InstallStatusResponse, InstallVersionStaging,
    InstallVersionStartRequest, LoaderBuildsRequest, LoaderInstallStartRequest,
};
use operation::{
    ContentDownloadFactAccumulator, InstallProgressCoalescer, assess_install_guardian_failure,
    begin_content_operation_journal, install_failure_evidence_from_download_error_or_facts,
    install_failure_evidence_from_download_facts, install_failure_point_from_journal,
    install_journal_is_terminal, install_progress_history_from_journal, install_progress_record,
    install_progress_with_terminal_error, interrupted_install_progress,
    observed_install_failure_progress, public_install_id, record_content_failure_outcome,
    record_content_operation_interrupted, record_content_operation_progress,
    record_install_guardian_terminal_outcome, record_install_operation_guardian_evidence,
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
pub use stream::{install_events_stream, loader_install_events_stream};

async fn start_install_version_with_foreground(
    state: &AppState,
    request: InstallVersionStartRequest,
    producer: &ProducerLease,
    inherited_foreground: Option<IntegrityForegroundLease>,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
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
    let foreground = match inherited_foreground {
        Some(foreground) => foreground,
        None => {
            register_install_foreground(state)?
                .wait_for_settlement()
                .await
        }
    };

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
        foreground,
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
    let foreground = InstallForegroundActivity::new_with_update_admission(
        reservation.hand_off(),
        update_admission,
    );
    let worker_foreground = foreground.clone();
    let interrupted_foreground = foreground.clone();
    let interrupted_state = state.clone();
    spawn_install_foreground_retention(
        state.clone(),
        install_id_task.clone(),
        producer.claim_child(),
        foreground,
    );
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
            let progress_tx_for_downloader = progress_tx.clone();
            let terminal_progress_for_downloader = Arc::clone(&terminal_progress);
            let mut install_facts = Vec::new();
            let install_result = {
                let install = downloader.install_version_with_facts(
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
                );
                tokio::pin!(install);
                tokio::select! {
                    result = &mut install => Some(result),
                    () = journal_failed.notified() => None,
                }
            };
            let Some(install_result) = install_result else {
                drop(progress_tx);
                let _ = finish_install_progress_task(store_task).await;
                return;
            };
            let attempt_terminal_progress = terminal_progress
                .lock()
                .ok()
                .and_then(|mut progress| progress.take());
            let (final_install_succeeded, final_terminal_progress) = match install_result {
                Ok(receipt) => {
                    let acceptance = match worker_foreground.retained() {
                        Some(foreground) => {
                            worker_state
                                .accept_known_good_install_receipt(
                                    &foreground,
                                    &installed_library_root,
                                    receipt,
                                )
                                .await
                        }
                        None => Err(std::io::Error::other(
                            "install foreground authority ended before receipt activation",
                        )),
                    };
                    match acceptance {
                        Ok(()) => (true, attempt_terminal_progress),
                        Err(error) => {
                            tracing::warn!(
                                operation_id = worker_operation_id.as_str(),
                                version_id = version_id.as_str(),
                                failure_kind = "known_good_reconciliation",
                                "install worker could not accept verified install authority"
                            );
                            let error = known_good_acceptance_download_error(error);
                            (
                                false,
                                Some(install_progress_with_terminal_error(
                                    terminal_failure_progress_or_default(attempt_terminal_progress),
                                    &error,
                                )),
                            )
                        }
                    }
                }
                Err(install_error) => {
                    tracing::warn!(
                        operation_id = worker_operation_id.as_str(),
                        version_id = version_id.as_str(),
                        failure_kind = install_error_log_kind(&install_error),
                        "install worker observed failed install"
                    );
                    record_install_failure_outcome_for_error(
                        worker_journals.as_ref(),
                        worker_failure_memory.as_ref(),
                        &worker_operation_id,
                        &install_error,
                        &install_facts,
                        &chrono::Utc::now().to_rfc3339(),
                    )
                    .await;
                    (
                        false,
                        Some(install_progress_with_terminal_error(
                            terminal_failure_progress_or_default(attempt_terminal_progress),
                            &install_error,
                        )),
                    )
                }
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
            let journal_committed = finish_install_progress_task(store_task).await;
            if journal_committed && let Some(summary) = failure_summary {
                emit_install_failed(worker_telemetry.as_ref(), &summary);
            }
        },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&interrupted_state, &interrupted_foreground).await;
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

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
        view_model: InstallProgressViewModel::starting(),
    })
}

pub(super) async fn record_install_failure_outcome(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    install_facts: &[ExecutionDownloadFact],
    observed_at: &str,
) {
    let evidence = install_failure_evidence_from_download_facts(operation_id, install_facts);
    record_install_failure_evidence(
        journals,
        failure_memory,
        operation_id,
        &evidence,
        observed_at,
    )
    .await;
}

pub(super) async fn record_install_failure_outcome_for_error(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    error: &DownloadError,
    install_facts: &[ExecutionDownloadFact],
    observed_at: &str,
) {
    let evidence =
        install_failure_evidence_from_download_error_or_facts(operation_id, error, install_facts);
    record_install_failure_evidence(
        journals,
        failure_memory,
        operation_id,
        &evidence,
        observed_at,
    )
    .await;
}

fn install_error_log_kind(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::FileOperation(_) => "file_operation",
        DownloadError::ResolveManifest(_) => "resolve_manifest",
        DownloadError::Request(_) => "request",
        DownloadError::ParseVersion(_) => "parse_version",
        DownloadError::PrepareRuntime(_) => "prepare_runtime",
        DownloadError::RuntimeSource(_) => "runtime_source",
        DownloadError::RuntimeUnavailableForPlatform { .. } => "runtime_unavailable_for_platform",
        DownloadError::RuntimeRosettaRequired { .. } => "runtime_rosetta_required",
        DownloadError::Integrity(_) => "integrity",
        DownloadError::LibraryPlan(_) => "library_plan",
    }
}

async fn record_install_failure_evidence(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    evidence: &[GuardianInstallArtifactFailureEvidence],
    observed_at: &str,
) {
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
        return;
    }
    let Some(assessment) = assess_install_guardian_failure(
        Some(failure_memory),
        operation_id,
        evidence,
        crate::state::contracts::OperationPhase::Downloading,
        observed_at,
    ) else {
        return;
    };
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
    }
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

async fn finish_install_progress_task(task: tokio::task::JoinHandle<bool>) -> bool {
    match task.await {
        Ok(true) => true,
        Ok(false) => false,
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
    let guardian = journal
        .as_ref()
        .and_then(install_guardian_outcome_summary_from_journal);
    let failure_view_model = install_failure_view_model(&view_model, guardian.as_ref());
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
    let producer = handoff
        .try_claim()
        .map_err(|_| install_shutdown_error_response())?;
    enqueue_install_with_dependency(state, request, None, None, producer).await
}

pub(crate) async fn enqueue_install_with_dependency(
    state: &AppState,
    request: InstallQueueRequest,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
    producer: ProducerLease,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    enqueue_install_with_dependency_admitted(
        state,
        request,
        prerequisite_queue_id,
        setup_cleanup,
        producer,
        update_admission,
    )
    .await
}

pub(crate) async fn enqueue_install_with_dependency_admitted(
    state: &AppState,
    request: InstallQueueRequest,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
    producer: ProducerLease,
    update_admission: UpdateOperationLease,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(
        state,
        request,
        InstallQueuePlacement::Back,
        prerequisite_queue_id,
        setup_cleanup,
        producer,
        update_admission,
    )
    .await
}

pub(crate) async fn enqueue_install_from_continuation(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    request: InstallQueueRequest,
    producer: ProducerLease,
    inherited_update_admission: Option<UpdateOperationLease>,
) -> Result<ContinuationInstallQueueResult, InstallApplicationError> {
    let _update_admission = match inherited_update_admission {
        Some(update_admission) => update_admission,
        None => state
            .try_admit_update_sensitive_operation()
            .map_err(install_update_admission_error_response)?,
    };
    let selection =
        enqueue_install_request(state, request, InstallQueuePlacement::Back, None, None).await?;
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
                let foreground = foreground.retained();
                async move {
                    start_queued_install(&state, &spec, &attempt_owner, Some(foreground)).await
                }
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
    let producer = handoff
        .try_claim()
        .map_err(|_| install_shutdown_error_response())?;
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    enqueue_install_with_placement(
        state,
        request,
        InstallQueuePlacement::Front,
        None,
        None,
        producer,
        update_admission,
    )
    .await
}

pub async fn remove_queued_install(
    state: &AppState,
    queue_id: &str,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let removed = state.installs().remove_queued_install(queue_id).await;
    let removed_instance_id = if let Some(QueuedInstallEntry {
        spec:
            InstallQueueSpec::Content {
                instance_id,
                action,
                ..
            },
        ..
    }) = removed.as_ref()
        && content_action_setup_cleanup(action).is_some()
    {
        remove_pristine_setup_instance(
            state,
            instance_id,
            content_action_setup_cleanup(action).expect("setup cleanup is present"),
        )
        .await
        .then(|| instance_id.clone())
    } else {
        None
    };
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
    let mut response = install_queue_state_response(state, notice, None).await;
    response.removed_instance_id = removed_instance_id;
    Ok(response)
}

fn content_action_owns_instance(action: &ContentQueueAction) -> bool {
    content_action_setup_cleanup(action).is_some()
}

fn content_action_setup_cleanup(action: &ContentQueueAction) -> Option<&SetupInstanceCleanup> {
    match action {
        ContentQueueAction::Install { setup_cleanup, .. }
        | ContentQueueAction::Modpack { setup_cleanup, .. } => setup_cleanup.as_ref(),
        ContentQueueAction::Uninstall { .. } => None,
    }
}

pub(crate) fn setup_instance_cleanup(
    state: &AppState,
    instance: &Instance,
    seed_shared_files: bool,
) -> SetupInstanceCleanup {
    let baseline = setup_instance_baseline(state, instance, seed_shared_files)
        .filter(|baseline| setup_instance_matches_baseline(state, baseline))
        .map(Box::new);
    SetupInstanceCleanup { baseline }
}

fn setup_instance_baseline(
    state: &AppState,
    instance: &Instance,
    seed_shared_files: bool,
) -> Option<SetupInstanceBaseline> {
    let mut paths = INSTANCE_LAYOUT_DIRS
        .iter()
        .map(|path| SetupInstancePathSnapshot {
            relative_path: PathBuf::from(path),
            kind: SetupInstancePathKind::Directory,
        })
        .collect::<Vec<_>>();
    if seed_shared_files && let Some(library_dir) = state.library_dir() {
        for file_name in SHARED_INSTANCE_FILES {
            let source = Path::new(&library_dir).join(file_name);
            let metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.is_file() => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return None,
            };
            paths.push(SetupInstancePathSnapshot {
                relative_path: PathBuf::from(file_name),
                kind: SetupInstancePathKind::File {
                    size: metadata.len(),
                    sha512: axial_content::sha512_file(&source).ok()?,
                },
            });
        }
    }
    paths.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Some(SetupInstanceBaseline {
        instance: instance.clone(),
        paths,
    })
}

fn setup_instance_matches_baseline(state: &AppState, baseline: &SetupInstanceBaseline) -> bool {
    if state.instances().get(&baseline.instance.id).as_ref() != Some(&baseline.instance) {
        return false;
    }
    setup_instance_paths_match(
        &state.instances().game_dir(&baseline.instance.id),
        &baseline.paths,
    )
}

fn setup_instance_paths_match(game_dir: &Path, expected: &[SetupInstancePathSnapshot]) -> bool {
    let Ok(root_metadata) = fs::symlink_metadata(game_dir) else {
        return false;
    };
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return false;
    }
    let expected: HashMap<&Path, &SetupInstancePathKind> = expected
        .iter()
        .map(|entry| (entry.relative_path.as_path(), &entry.kind))
        .collect();
    let mut seen = HashSet::new();
    let mut pending = vec![game_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(game_dir) else {
                return false;
            };
            let Some(expected_kind) = expected.get(relative) else {
                return false;
            };
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                return false;
            };
            if metadata.file_type().is_symlink() || !seen.insert(relative.to_path_buf()) {
                return false;
            }
            match expected_kind {
                SetupInstancePathKind::Directory if metadata.is_dir() => pending.push(path),
                SetupInstancePathKind::File { size, sha512 }
                    if metadata.is_file()
                        && metadata.len() == *size
                        && axial_content::sha512_file(&path).ok().as_ref() == Some(sha512) => {}
                _ => return false,
            }
        }
    }
    seen.len() == expected.len()
}

/// Remove an untouched instance created solely for setup, but only while no
/// launch or content mutation can be using it. Any metadata or filesystem
/// difference is treated as user ownership and retains the instance.
pub(crate) async fn remove_pristine_setup_instance(
    state: &AppState,
    instance_id: &str,
    cleanup: &SetupInstanceCleanup,
) -> bool {
    let Ok(update_admission) = state.try_admit_update_sensitive_operation() else {
        return false;
    };
    remove_pristine_setup_instance_admitted(state, instance_id, cleanup, &update_admission).await
}

pub(crate) async fn remove_pristine_setup_instance_admitted(
    state: &AppState,
    instance_id: &str,
    cleanup: &SetupInstanceCleanup,
    _update_admission: &UpdateOperationLease,
) -> bool {
    let Ok(foreground) = state.register_integrity_foreground() else {
        return false;
    };
    let foreground = foreground.wait_for_settlement().await;
    state
        .delete_pristine_setup_instance(&foreground, instance_id.to_string(), cleanup)
        .await
        .unwrap_or(false)
}

async fn enqueue_install_with_placement(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
    producer: ProducerLease,
    _update_admission: UpdateOperationLease,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let selection = enqueue_install_request(
        state,
        request,
        placement,
        prerequisite_queue_id,
        setup_cleanup,
    )
    .await?;
    let selected_queue_id = install_queue_outcome_id(&selection.outcome).to_string();
    let owns_selected_queue = matches!(
        &selection.outcome,
        InstallQueueEnqueueOutcome::Enqueued { .. }
            | InstallQueueEnqueueOutcome::MovedToFront { .. }
    );
    let start_state = state.clone();
    let started = maybe_start_selected_queued_install_owned_with(
        state,
        &selected_queue_id,
        owns_selected_queue,
        |spec| {
            let state = start_state.clone();
            let attempt_owner = producer.claim_child();
            async move { start_queued_install(&state, &spec, &attempt_owner, None).await }
        },
    )
    .await?;
    if let Some(started) = started.as_ref() {
        spawn_install_queue_monitor_owned(state.clone(), started.install_id.clone(), producer);
    }
    Ok(install_queue_state_response(state, Some(selection.notice), started).await)
}

async fn enqueue_install_request(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
) -> Result<InstallQueueSelection, InstallApplicationError> {
    let spec =
        install_queue_spec_from_request(state, request, prerequisite_queue_id, setup_cleanup)
            .await?;
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
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
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
        InstallQueueRequest::Content {
            instance_id,
            label,
            action,
        } => {
            let instance_id = instance_id.trim().to_string();
            if instance_id.is_empty() || state.instances().get(&instance_id).is_none() {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "instance not found" })),
                ));
            }
            let action = match action {
                InstallQueueContentActionRequest::Install {
                    selections,
                    allow_incompatible,
                } => {
                    if selections.is_empty() || selections.len() > 40 {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "content selections must contain between 1 and 40 items"
                            })),
                        ));
                    }
                    ContentQueueAction::Install {
                        selections: selections
                            .into_iter()
                            .map(|selection| QueuedContentSelection {
                                canonical_id: selection.canonical_id.trim().to_string(),
                                kind: selection.kind,
                                version_id: selection
                                    .version_id
                                    .filter(|value| !value.trim().is_empty()),
                            })
                            .collect(),
                        allow_incompatible,
                        setup_cleanup,
                    }
                }
                InstallQueueContentActionRequest::Uninstall { canonical_ids } => {
                    let canonical_ids = canonical_ids
                        .into_iter()
                        .map(|canonical_id| canonical_id.trim().to_string())
                        .filter(|canonical_id| !canonical_id.is_empty())
                        .collect::<HashSet<_>>();
                    if canonical_ids.is_empty() || canonical_ids.len() > 500 {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "canonical_ids must contain between 1 and 500 items"
                            })),
                        ));
                    }
                    let mut canonical_ids = canonical_ids.into_iter().collect::<Vec<_>>();
                    canonical_ids.sort();
                    ContentQueueAction::Uninstall { canonical_ids }
                }
                InstallQueueContentActionRequest::Modpack {
                    canonical_id,
                    version_id,
                    selected_file_ids,
                    include_overrides,
                } => {
                    let canonical_id = canonical_id.trim().to_string();
                    let version_id = version_id.trim().to_string();
                    if canonical_id.is_empty() || version_id.is_empty() {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": "invalid modpack operation" })),
                        ));
                    }
                    crate::application::content::validate_modpack_file_selection_ids(
                        &selected_file_ids,
                    )?;
                    ContentQueueAction::Modpack {
                        canonical_id,
                        version_id,
                        selected_file_ids,
                        include_overrides,
                        setup_cleanup,
                    }
                }
            };
            let label = label.trim();
            let label = if label.is_empty() {
                "Instance content".to_string()
            } else {
                label.chars().take(120).collect()
            };
            Ok(InstallQueueSpec::Content {
                instance_id,
                label,
                action,
                prerequisite_queue_id,
            })
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
    maybe_start_next_queued_install_owned(state, &producer).await
}

async fn maybe_start_next_queued_install_owned(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    maybe_start_next_queued_install_owned_with(
        state,
        producer,
        |state, spec, producer| async move {
            start_queued_install(&state, &spec, &producer, None).await
        },
    )
    .await
}

async fn maybe_start_next_queued_install_owned_with<Start, StartFuture>(
    state: &AppState,
    producer: &ProducerLease,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnOnce(AppState, InstallQueueSpec, ProducerLease) -> StartFuture + Send + 'static,
    StartFuture:
        Future<Output = Result<InstallStartResponse, InstallApplicationError>> + Send + 'static,
{
    let transaction_state = state.clone();
    let transaction_producer = producer.claim_child();
    let transaction_owner = transaction_producer.claim_child();
    let transaction = transaction_owner.spawn_joinable(async move {
        let started = start_next_queued_install_transaction_with(
            &transaction_state,
            &transaction_producer,
            start,
        )
        .await?;
        if let Some(started) = started.as_ref() {
            spawn_install_queue_monitor_owned(
                transaction_state,
                started.install_id.clone(),
                transaction_producer,
            );
        }
        Ok(started)
    });
    transaction
        .await
        .map_err(|_| install_queue_start_stopped_error_response())?
}

async fn start_next_queued_install_transaction(
    state: &AppState,
    producer: &ProducerLease,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    start_next_queued_install_transaction_with(
        state,
        producer,
        |state, spec, producer| async move {
            start_queued_install(&state, &spec, &producer, None).await
        },
    )
    .await
}

async fn start_next_queued_install_transaction_with<Start, StartFuture>(
    state: &AppState,
    producer: &ProducerLease,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnOnce(AppState, InstallQueueSpec, ProducerLease) -> StartFuture,
    StartFuture: Future<Output = Result<InstallStartResponse, InstallApplicationError>>,
{
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let _queue_start = state.installs().acquire_queue_start_gate().await;
    let entry = loop {
        let Some(entry) = state.installs().reserve_next_queued_install().await else {
            return Ok(None);
        };
        if settle_unmet_queue_prerequisite(state, &entry, &update_admission).await {
            continue;
        }
        break entry;
    };
    let started = match start(state.clone(), entry.spec.clone(), producer.claim_child()).await {
        Ok(started) => started,
        Err(error) => {
            state
                .installs()
                .complete_reserved_queued_install(&entry.queue_id, false)
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
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let _queue_start = state.installs().acquire_queue_start_gate().await;
    let initial_pending = state.installs().queue_snapshot().await.pending.len();
    for _ in 0..initial_pending.saturating_add(1) {
        let Some(entry) = state.installs().reserve_next_queued_install().await else {
            return selected_queue_residual(state, selected_queue_id, owns_selected_queue).await;
        };
        if settle_unmet_queue_prerequisite(state, &entry, &update_admission).await {
            if entry.queue_id == selected_queue_id {
                return Err(selected_queue_missing_error_response());
            }
            continue;
        }
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
                    .complete_reserved_queued_install(&entry.queue_id, false)
                    .await;
                if entry.queue_id == selected_queue_id {
                    return Err(error);
                }
            }
        }
    }
    selected_queue_residual(state, selected_queue_id, owns_selected_queue).await
}

async fn settle_unmet_queue_prerequisite(
    state: &AppState,
    entry: &QueuedInstallEntry,
    update_admission: &UpdateOperationLease,
) -> bool {
    let prerequisite = match &entry.spec {
        InstallQueueSpec::Content {
            prerequisite_queue_id,
            ..
        } => prerequisite_queue_id.as_deref(),
        _ => None,
    };
    let Some(prerequisite) = prerequisite else {
        return false;
    };
    if state
        .installs()
        .queued_install_succeeded(prerequisite)
        .await
        == Some(true)
    {
        return false;
    }
    state
        .installs()
        .complete_reserved_queued_install(&entry.queue_id, false)
        .await;
    if let InstallQueueSpec::Content {
        instance_id,
        action,
        ..
    } = &entry.spec
        && let Some(cleanup) = content_action_setup_cleanup(action)
    {
        let _ =
            remove_pristine_setup_instance_admitted(state, instance_id, cleanup, update_admission)
                .await;
    }
    true
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
    foreground: Option<IntegrityForegroundLease>,
) -> Result<InstallStartResponse, InstallApplicationError> {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => {
            start_install_version_with_foreground(
                state,
                InstallVersionStartRequest {
                    version_id: version_id.clone(),
                },
                producer,
                foreground,
            )
            .await
        }
        InstallQueueSpec::Loader {
            component_id,
            build_id,
            ..
        } => {
            start_loader_install_with_foreground(
                state,
                LoaderInstallStartRequest {
                    component_id: *component_id,
                    build_id: build_id.clone(),
                },
                producer,
                foreground,
            )
            .await
        }
        InstallQueueSpec::Content {
            instance_id,
            label,
            action,
            ..
        } => start_content_operation(state, instance_id, label, action, producer).await,
    }
}

async fn start_content_operation(
    state: &AppState,
    instance_id: &str,
    label: &str,
    action: &ContentQueueAction,
    producer: &ProducerLease,
) -> Result<InstallStartResponse, InstallApplicationError> {
    start_content_operation_with_after_journal(
        state,
        instance_id,
        label,
        action,
        producer,
        |_, _| async {},
    )
    .await
}

async fn start_content_operation_with_after_journal<AfterJournal, AfterJournalFuture>(
    state: &AppState,
    instance_id: &str,
    label: &str,
    action: &ContentQueueAction,
    producer: &ProducerLease,
    after_journal: AfterJournal,
) -> Result<InstallStartResponse, InstallApplicationError>
where
    AfterJournal: FnOnce(String, OperationId) -> AfterJournalFuture,
    AfterJournalFuture: Future<Output = ()>,
{
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let install_id = generate_install_id("content");
    let operation_id = install_operation_id(&install_id);
    let initialization = begin_content_journal_with_owned_reconciliation(
        state.installs().clone(),
        state.journals().clone(),
        install_id.clone(),
        operation_id.clone(),
        instance_id.to_string(),
        producer,
    )
    .await
    .map_err(|_| install_journal_error_response())?;
    after_journal(install_id.clone(), operation_id.clone()).await;
    state.installs().insert(install_id.clone()).await;
    let worker_state = state.clone();
    let worker_store = state.installs().clone();
    let worker_journals = state.journals().clone();
    let progress_journals = worker_journals.clone();
    let worker_install_id = install_id.clone();
    let worker_operation_id = operation_id.clone();
    let progress_operation_id = operation_id.clone();
    let worker_instance_id = instance_id.to_string();
    let worker_action = action.clone();
    let download_facts = Arc::new(Mutex::new(ContentDownloadFactAccumulator::default()));
    let worker_read_owner = producer.claim_child();
    let progress_owner = producer.claim_child();
    let interrupted_state = state.clone();
    let interrupted_journals = state.journals().clone();
    let interrupted_operation_id = operation_id.clone();
    let interrupted_download_facts = download_facts.clone();
    let attempted_terminal = Arc::new(Mutex::new(
        None::<(
            DownloadProgress,
            Option<crate::application::content::ContentExecutionFailureKind>,
        )>,
    ));
    let interrupted_attempted_terminal = attempted_terminal.clone();
    let interrupted_instance_id = instance_id.to_string();
    let interrupted_setup_cleanup = content_action_setup_cleanup(action).cloned();
    InstallStore::spawn_tracked_worker_with_interrupt_progress_owned(
        state.installs().clone(),
        producer.claim_child(),
        install_id.clone(),
        content_interrupted_progress(false),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let progress_store = worker_store.clone();
            let progress_install_id = worker_install_id.clone();
            let progress_task = {
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        if progress.done {
                            continue;
                        }
                        if record_content_operation_progress(
                            &progress_journals,
                            &progress_operation_id,
                            &progress,
                            &[],
                            &mut last_journal_phase,
                        )
                        .await
                        .is_err()
                        {
                            tracing::warn!("failed to record content operation journal progress");
                            journal_failed.notify_one();
                            return false;
                        }
                        progress_store
                            .emit(&progress_install_id, sanitize_install_progress(progress))
                            .await;
                    }
                    true
                })
            };

            let content_operation = async {
                if content_action_owns_instance(&worker_action)
                    && !instance_version_is_installed_and_launchable(
                        &worker_state,
                        &worker_read_owner,
                        &worker_instance_id,
                    )
                    .await
                {
                    Err(crate::application::content::ContentExecutionError::from((
                        StatusCode::PRECONDITION_FAILED,
                        Json(serde_json::json!({
                            "error": "Minecraft or the selected mod loader did not finish installing."
                        })),
                    )))
                } else {
                    match &worker_action {
                        ContentQueueAction::Install {
                            selections,
                            allow_incompatible,
                            ..
                        } => {
                            let request = crate::application::content::ContentInstallRequest {
                                instance_id: worker_instance_id.clone(),
                                selections: selections
                                    .iter()
                                    .map(|selection| {
                                        crate::application::content::ContentSelection {
                                            canonical_id: selection.canonical_id.clone(),
                                            kind: selection.kind,
                                            version_id: selection.version_id.clone(),
                                        }
                                    })
                                    .collect(),
                                allow_incompatible: *allow_incompatible,
                            };
                            crate::application::content::execute_content_install(
                                &worker_state,
                                request,
                                |progress| {
                                    let _ = progress_tx.send(progress);
                                },
                                |fact| {
                                    download_facts
                                        .lock()
                                        .expect("content download fact accumulator lock poisoned")
                                        .record(fact);
                                },
                            )
                            .await
                        }
                        ContentQueueAction::Uninstall { canonical_ids } => {
                            let _ = progress_tx.send(content_progress(
                                "removing",
                                0,
                                canonical_ids.len() as i32,
                                false,
                                None,
                            ));
                            crate::application::content::execute_content_uninstalls(
                                &worker_state,
                                &worker_instance_id,
                                canonical_ids,
                            )
                            .await
                        }
                        ContentQueueAction::Modpack {
                            canonical_id,
                            version_id,
                            selected_file_ids,
                            include_overrides,
                            ..
                        } => crate::application::content::execute_modpack_install(
                            &worker_state,
                            crate::application::content::ModpackInstallRequest {
                                instance_id: worker_instance_id.clone(),
                                canonical_id: canonical_id.clone(),
                                version_id: Some(version_id.clone()),
                                selected_file_ids: selected_file_ids.clone(),
                                include_overrides: *include_overrides,
                            },
                            |progress| {
                                let _ = progress_tx.send(progress);
                            },
                            |fact| {
                                download_facts
                                    .lock()
                                    .expect("content download fact accumulator lock poisoned")
                                    .record(fact);
                            },
                        )
                        .await
                        .map(|_| ()),
                    }
                }
            };
            let mut content_operation = Box::pin(content_operation);
            let result = tokio::select! {
                result = content_operation.as_mut() => Some(result),
                () = journal_failed.notified() => None,
            };
            drop(content_operation);
            let Some(result) = result else {
                drop(progress_tx);
                let _ = finish_install_progress_task(progress_task).await;
                return;
            };

            let (terminal, failure_kind) = match result {
                Ok(()) => (content_progress("done", 1, 1, true, None), None),
                Err(error) => {
                    let ((_, Json(body)), failure_kind) = error.into_parts();
                    let removed_instance = match content_action_setup_cleanup(&worker_action) {
                        Some(cleanup) => {
                            remove_pristine_setup_instance(
                                &worker_state,
                                &worker_instance_id,
                                cleanup,
                            )
                            .await
                        }
                        None => false,
                    };
                    let mut message = body
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(INSTALL_FAILURE_MESSAGE)
                        .to_string();
                    if removed_instance {
                        message.push_str(" The incomplete setup instance was removed.");
                    }
                    (
                        content_progress(
                            if removed_instance {
                                CONTENT_INSTANCE_REMOVED_PHASE
                            } else {
                                "error"
                            },
                            0,
                            1,
                            true,
                            Some(message),
                        ),
                        failure_kind,
                    )
                }
            };
            drop(progress_tx);
            if !finish_install_progress_task(progress_task).await {
                return;
            }
            let (facts, journal_facts) = {
                let download_facts = download_facts
                    .lock()
                    .expect("content download fact accumulator lock poisoned");
                (download_facts.facts(), download_facts.journal_facts())
            };
            *attempted_terminal
                .lock()
                .expect("content terminal progress lock poisoned") =
                Some((terminal.clone(), failure_kind));
            if commit_and_emit_content_terminal_progress(
                worker_store.as_ref(),
                worker_journals.as_ref(),
                worker_state.failure_memory(),
                ContentTerminalProgress {
                    operation_id: &worker_operation_id,
                    install_id: &worker_install_id,
                    progress: terminal,
                    execution_facts: &facts,
                    journal_facts: &journal_facts,
                    failure_kind,
                },
            )
            .await
            .is_err()
            {
                tracing::warn!("failed to durably settle content terminal progress");
            }
        },
        move |_| async move {
            let _update_admission = update_admission;
            let progress = interrupted_content_progress(
                &interrupted_state,
                &interrupted_instance_id,
                interrupted_setup_cleanup.as_ref(),
            )
            .await;
            let (facts, journal_facts) = {
                let download_facts = interrupted_download_facts
                    .lock()
                    .expect("content download fact accumulator lock poisoned");
                (download_facts.facts(), download_facts.journal_facts())
            };
            let attempted_terminal = interrupted_attempted_terminal
                .lock()
                .expect("content terminal progress lock poisoned")
                .clone();
            settle_content_worker_interruption(
                &interrupted_journals,
                interrupted_state.failure_memory(),
                &interrupted_operation_id,
                progress,
                &journal_facts,
                &facts,
                attempted_terminal,
            )
            .await
        },
    );
    initialization.hand_off();

    Ok(InstallStartResponse {
        install_id,
        operation_id,
        view_model: InstallProgressViewModel {
            phase_id: "starting".to_string(),
            label: format!("Preparing {label}"),
            progress_pct: 0,
            terminal: false,
            failed: false,
            active_step: None,
        },
    })
}

struct ContentTerminalProgress<'a> {
    operation_id: &'a OperationId,
    install_id: &'a str,
    progress: DownloadProgress,
    execution_facts: &'a [axial_minecraft::download::ExecutionDownloadFact],
    journal_facts: &'a [String],
    failure_kind: Option<crate::application::content::ContentExecutionFailureKind>,
}

async fn commit_and_emit_content_terminal_progress(
    store: &InstallStore,
    journals: &OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    terminal: ContentTerminalProgress<'_>,
) -> Result<(), OperationJournalStoreError> {
    if terminal.progress.error.is_some() {
        let (evidence, phase) = terminal
            .failure_kind
            .map(|kind| content_execution_failure_evidence(terminal.operation_id, kind))
            .map_or((None, OperationPhase::Downloading), |(evidence, phase)| {
                (Some(evidence), phase)
            });
        record_content_failure_outcome(
            journals,
            failure_memory,
            terminal.operation_id,
            terminal.execution_facts,
            evidence,
            phase,
            &chrono::Utc::now().to_rfc3339(),
        )
        .await?;
    }
    let mut last_journal_phase = None;
    record_content_operation_progress(
        journals,
        terminal.operation_id,
        &terminal.progress,
        terminal.journal_facts,
        &mut last_journal_phase,
    )
    .await?;
    store
        .emit(
            terminal.install_id,
            sanitize_install_progress(terminal.progress),
        )
        .await;
    Ok(())
}

async fn settle_content_worker_interruption(
    journals: &OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &OperationId,
    fallback: DownloadProgress,
    journal_facts: &[String],
    execution_facts: &[axial_minecraft::download::ExecutionDownloadFact],
    attempted_terminal: Option<(
        DownloadProgress,
        Option<crate::application::content::ContentExecutionFailureKind>,
    )>,
) -> Option<DownloadProgress> {
    loop {
        if let Some((attempted_progress, failure_kind)) = attempted_terminal.as_ref()
            && attempted_progress.error.is_some()
        {
            let (evidence, phase) = failure_kind
                .map(|kind| content_execution_failure_evidence(operation_id, kind))
                .map_or((None, OperationPhase::Downloading), |(evidence, phase)| {
                    (Some(evidence), phase)
                });
            if let Err(error) = record_content_failure_outcome(
                journals,
                failure_memory,
                operation_id,
                execution_facts,
                evidence,
                phase,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
            {
                if matches!(
                    error,
                    OperationJournalStoreError::Persistence(_)
                        | OperationJournalStoreError::RetryRequired
                ) {
                    tokio::time::sleep(INSTALL_JOURNAL_RETRY_MAX_DELAY).await;
                    continue;
                }
                tracing::warn!("failed to settle interrupted content Guardian outcome");
                return None;
            }
        }
        match record_content_operation_interrupted(
            journals,
            operation_id,
            &fallback,
            journal_facts,
            execution_facts,
        )
        .await
        {
            Ok(()) => return Some(fallback.clone()),
            Err(error) => {
                if let Some((attempted_progress, _)) = attempted_terminal.as_ref()
                    && journals.get(operation_id).as_ref().is_some_and(|entry| {
                        operation::content_terminal_progress_is_visible(
                            entry,
                            operation_id,
                            attempted_progress,
                            journal_facts,
                        )
                    })
                {
                    return Some(sanitize_install_progress(attempted_progress.clone()));
                }
                if matches!(
                    error,
                    OperationJournalStoreError::Persistence(_)
                        | OperationJournalStoreError::RetryRequired
                ) {
                    tokio::time::sleep(INSTALL_JOURNAL_RETRY_MAX_DELAY).await;
                    continue;
                }
                tracing::warn!("failed to record interrupted content operation journal");
                return None;
            }
        }
    }
}

fn content_execution_failure_evidence(
    operation_id: &OperationId,
    kind: crate::application::content::ContentExecutionFailureKind,
) -> (GuardianInstallArtifactFailureEvidence, OperationPhase) {
    use crate::application::content::ContentExecutionFailureKind;
    let (target, failure_kind, phase) = match kind {
        ContentExecutionFailureKind::FileOperation => (
            "content_filesystem",
            crate::guardian::GuardianInstallArtifactFailureKind::TempWriteFailed,
            OperationPhase::Installing,
        ),
        ContentExecutionFailureKind::MetadataInvalid => (
            "content_metadata",
            crate::guardian::GuardianInstallArtifactFailureKind::MetadataInvalid,
            OperationPhase::Downloading,
        ),
        ContentExecutionFailureKind::NetworkFailure => (
            "content_download",
            crate::guardian::GuardianInstallArtifactFailureKind::NetworkFailure,
            OperationPhase::Downloading,
        ),
        ContentExecutionFailureKind::PermissionDenied => (
            "content_filesystem",
            crate::guardian::GuardianInstallArtifactFailureKind::PermissionDenied,
            OperationPhase::Installing,
        ),
        ContentExecutionFailureKind::ProviderFailure => (
            "content_provider",
            crate::guardian::GuardianInstallArtifactFailureKind::ProviderFailure,
            OperationPhase::Downloading,
        ),
    };
    (
        GuardianInstallArtifactFailureEvidence::launcher_managed(
            Some(operation_id.clone()),
            target,
            failure_kind,
        ),
        phase,
    )
}

fn content_progress(
    phase: &str,
    current: i32,
    total: i32,
    done: bool,
    error: Option<String>,
) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file: None,
        error,
        done,
        bytes_done: None,
        bytes_total: None,
    }
}

async fn interrupted_content_progress(
    state: &AppState,
    instance_id: &str,
    setup_cleanup: Option<&SetupInstanceCleanup>,
) -> DownloadProgress {
    let removed = match setup_cleanup {
        Some(cleanup) => remove_pristine_setup_instance(state, instance_id, cleanup).await,
        None => false,
    };
    content_interrupted_progress(removed)
}

fn content_interrupted_progress(removed_instance: bool) -> DownloadProgress {
    content_progress(
        if removed_instance {
            CONTENT_INSTANCE_REMOVED_PHASE
        } else {
            "error"
        },
        0,
        1,
        true,
        Some(if removed_instance {
            "Content operation stopped. The incomplete setup instance was removed.".to_string()
        } else {
            "Content operation stopped before completing. Try again.".to_string()
        }),
    )
}

fn spawn_install_queue_monitor_owned(state: AppState, install_id: String, producer: ProducerLease) {
    let successor_owner = producer.claim_child();
    producer.spawn(async move {
        let mut install_id = install_id;
        let mut shutdown = state.subscribe_shutdown();
        loop {
            let succeeded = wait_for_install_terminal(&state, &install_id).await;
            state.invalidate_installed_versions();
            state
                .installs()
                .complete_active_queued_install(&install_id, succeeded)
                .await;
            if *shutdown.borrow_and_update() {
                return;
            }
            let Ok(successor) = successor_owner.try_claim_successor() else {
                return;
            };
            let Ok(Some(started_install)) =
                start_next_queued_install_transaction(&state, &successor).await
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

fn install_update_admission_error_response(
    error: UpdateOperationAdmissionError,
) -> InstallApplicationError {
    let message = match error {
        UpdateOperationAdmissionError::ApplyInProgress => {
            "Installs are unavailable while an update is being applied."
        }
        UpdateOperationAdmissionError::RestartPending => {
            "Restart Axial to finish the applied update before starting installs."
        }
    };
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": message })),
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

async fn wait_for_install_terminal(state: &AppState, install_id: &str) -> bool {
    let Some((snapshot, mut receiver)) = state.installs().subscribe_records(install_id).await
    else {
        return false;
    };
    if snapshot.done
        || snapshot
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.done)
    {
        return snapshot
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.error.is_none());
    }
    loop {
        match receiver.recv().await {
            Ok(record) if record.progress.done => return record.progress.error.is_none(),
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return false,
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
        removed_instance_id: None,
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
    let active_queued_count_label = (queued_count > 0).then(|| format!(", {queued_count_label}"));
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
        InstallQueueSpec::Content { .. } => "content",
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
        InstallQueueSpec::Content { label, .. } => label.clone(),
    }
}

fn install_queue_install_item(spec: &InstallQueueSpec) -> InstallQueueInstallItemViewModel {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => InstallQueueInstallItemViewModel {
            version_id: version_id.clone(),
            loader: None,
            content: None,
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
            content: None,
        },
        InstallQueueSpec::Content {
            instance_id,
            action,
            ..
        } => InstallQueueInstallItemViewModel {
            version_id: instance_id.clone(),
            loader: None,
            content: Some(InstallQueueContentItemViewModel {
                instance_id: instance_id.clone(),
                action: content_action_request(action),
            }),
        },
    }
}

fn content_action_request(action: &ContentQueueAction) -> InstallQueueContentActionRequest {
    match action {
        ContentQueueAction::Install {
            selections,
            allow_incompatible,
            ..
        } => InstallQueueContentActionRequest::Install {
            selections: selections
                .iter()
                .map(|selection| InstallQueueContentSelection {
                    canonical_id: selection.canonical_id.clone(),
                    kind: selection.kind,
                    version_id: selection.version_id.clone(),
                })
                .collect(),
            allow_incompatible: *allow_incompatible,
        },
        ContentQueueAction::Uninstall { canonical_ids } => {
            InstallQueueContentActionRequest::Uninstall {
                canonical_ids: canonical_ids.clone(),
            }
        }
        ContentQueueAction::Modpack {
            canonical_id,
            version_id,
            selected_file_ids,
            include_overrides,
            ..
        } => InstallQueueContentActionRequest::Modpack {
            canonical_id: canonical_id.clone(),
            version_id: version_id.clone(),
            selected_file_ids: selected_file_ids.clone(),
            include_overrides: *include_overrides,
        },
    }
}

pub(crate) fn effective_install_version_id(request: &InstallVersionStartRequest) -> String {
    request.version_id.trim().to_string()
}

fn install_failure_view_model(
    progress: &InstallProgressViewModel,
    guardian: Option<&GuardianInstallOutcomeSummary>,
) -> Option<InstallFailureViewModel> {
    if !progress.failed {
        return None;
    }

    let summary = guardian
        .map(|guardian| guardian.label().to_string())
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
    Some(InstallFailureViewModel {
        state_id: if progress.phase_id == CONTENT_INSTANCE_REMOVED_PHASE {
            "failed_instance_removed".to_string()
        } else {
            failure_state_id(guardian).to_string()
        },
        title: "Install failed".to_string(),
        tone: "err".to_string(),
        detail: details.first().cloned(),
        details,
        retry_action: install_retry_action(progress, guardian),
        dismiss_action: InstallActionViewModel {
            action: "dismiss".to_string(),
            label: "Dismiss".to_string(),
            enabled: true,
            disabled_reason: None,
        },
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

fn failure_state_id(guardian: Option<&GuardianInstallOutcomeSummary>) -> &'static str {
    match guardian.map(GuardianInstallOutcomeSummary::decision) {
        Some("retry") => "failed_retryable",
        Some("block") => "failed_blocked",
        Some("suppress") => "failed_suppressed",
        Some(_) => "failed_guardian_recorded",
        None => "failed",
    }
}

fn install_retry_action(
    progress: &InstallProgressViewModel,
    guardian: Option<&GuardianInstallOutcomeSummary>,
) -> InstallActionViewModel {
    if progress.phase_id == CONTENT_INSTANCE_REMOVED_PHASE {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: false,
            disabled_reason: Some(
                "The temporary setup instance was removed. Create the instance again to retry."
                    .to_string(),
            ),
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
    matches!(
        guardian.diagnosis_id(),
        DiagnosisId::ManagedRuntimeRosettaRequired | DiagnosisId::LauncherManagedArtifactCorrupt
    )
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
