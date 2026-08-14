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

use crate::application::instances::instance_version_is_installed_and_launchable;
use crate::execution::{ExecutionFact, ExecutionFactKind};
use crate::guardian::{DiagnosisId, GuardianInstallOutcomeSummary};
use crate::observability::{
    operation_journal_proof_record,
    telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
    },
};
use crate::state::AppState;
use crate::state::contracts::{OperationId, OperationJournalEntry, OperationPhase};
use crate::state::{
    ActiveQueuedInstallEntry, ContentQueueAction, InstallAdmissionMarker,
    InstallInitializationStatus, InstallQueueAdmission, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallQueueStartAuthority,
    InstallQueueStartFailureDisposition, InstallQueueStartGuard, InstallQueueStartReconciliation,
    InstallStore, IntegrityForegroundLease, IntegrityForegroundRegistration,
    ManagedLibraryAvailability, OperationJournalReconciliation, OperationJournalStore,
    OperationJournalStoreError, ProducerLease, QueuedContentSelection, QueuedInstallEntry,
    RequestProducerHandoff, SetupInstanceBaseline, SetupInstanceCleanup, SetupInstancePathKind,
    SetupInstancePathSnapshot, UpdateOperationAdmissionError, UpdateOperationLease,
    operation_journal_plan_is_visible,
};
use axial_config::{INSTANCE_LAYOUT_DIRS, Instance, SHARED_INSTANCE_FILES};
use axial_minecraft::managed_path::ManagedLibraryOperation;
use axial_minecraft::{
    DownloadError, DownloadProgress, Downloader, ManagedInstallAcknowledgementOutcome,
    ManagedInstallCommittedEvidence, ManagedInstallDurableOutcome,
    ManagedInstallRolledBackEvidence, classify_managed_install_publication,
    download::ExecutionDownloadFact, resolve_build_record_for_install,
    verify_managed_install_publication_evidence_root,
    verify_managed_install_reconstruction_checkpoint,
};
use axum::{Json, http::StatusCode};
use std::collections::HashSet;
use std::fs;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::{mpsc, oneshot};

async fn await_managed_install_settlement_retaining<Authority, Install, JournalFailure>(
    authority: Authority,
    install: Install,
    journal_failure: JournalFailure,
) -> (Install::Output, Authority)
where
    Install: Future,
    JournalFailure: Future<Output = ()>,
{
    tokio::pin!(install);
    tokio::pin!(journal_failure);
    let result = tokio::select! {
        result = &mut install => result,
        () = &mut journal_failure => {
            install.await
        }
    };
    (result, authority)
}

type InstallRequestDrain = Pin<Box<dyn Future<Output = ()> + Send>>;

#[derive(Debug)]
pub(super) enum InstallQueueStartFailure {
    Application(InstallApplicationError),
    BlockedByLiveSession,
}

impl From<InstallApplicationError> for InstallQueueStartFailure {
    fn from(error: InstallApplicationError) -> Self {
        Self::Application(error)
    }
}

enum ManagedPublicationConvergence<T, E> {
    Settled(Result<T, E>),
    DeferredNonterminal,
}

enum DurableInstallPublication {
    Committed,
    RolledBack(DownloadError),
    Refused(DownloadError),
    DeferredNonterminal,
}

pub(super) fn managed_install_publication_mismatch_error() -> DownloadError {
    DownloadError::FileOperation(std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        "managed install publication belongs to another operation",
    ))
}

async fn wait_for_managed_publication_retry(
    request_drain: &mut InstallRequestDrain,
    delay: Duration,
) -> bool {
    tokio::select! {
        biased;
        _ = request_drain.as_mut() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

async fn converge_managed_install_publication(
    mut recovery: axial_minecraft::download::ManagedInstallPublicationRecovery,
    request_drain: &mut InstallRequestDrain,
    defer_on_request_drain: bool,
) -> ManagedPublicationConvergence<axial_minecraft::KnownGoodInstallReceipt, DownloadError> {
    let mut retry_delay = Duration::from_millis(100);
    let maximum_retry_delay = Duration::from_secs(5);
    if defer_on_request_drain
        && !wait_for_managed_publication_retry(request_drain, Duration::ZERO).await
    {
        return ManagedPublicationConvergence::DeferredNonterminal;
    }
    loop {
        match recovery.retry().await {
            Err(DownloadError::PublicationIndeterminate(next)) => {
                recovery = next;
                if defer_on_request_drain {
                    if !wait_for_managed_publication_retry(request_drain, retry_delay).await {
                        return ManagedPublicationConvergence::DeferredNonterminal;
                    }
                } else {
                    tokio::time::sleep(retry_delay).await;
                }
                retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
            }
            settled => return ManagedPublicationConvergence::Settled(settled),
        }
    }
}

async fn converge_managed_install_durable_outcome(
    mut outcome: ManagedInstallDurableOutcome,
    request_drain: &mut InstallRequestDrain,
) -> Option<ManagedInstallDurableOutcome> {
    let mut retry_delay = Duration::from_millis(100);
    let maximum_retry_delay = Duration::from_secs(5);
    loop {
        match outcome {
            ManagedInstallDurableOutcome::Indeterminate(recovery) => {
                if !wait_for_managed_publication_retry(request_drain, retry_delay).await {
                    return None;
                }
                outcome = recovery.retry().await;
                retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
            }
            settled => return Some(settled),
        }
    }
}

async fn converge_managed_install_acknowledgement(
    mut outcome: ManagedInstallAcknowledgementOutcome,
    request_drain: &mut InstallRequestDrain,
) -> bool {
    let mut retry_delay = Duration::from_millis(100);
    let maximum_retry_delay = Duration::from_secs(5);
    loop {
        match outcome {
            ManagedInstallAcknowledgementOutcome::Acknowledged => return true,
            ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                if !wait_for_managed_publication_retry(request_drain, retry_delay).await {
                    return false;
                }
                outcome = recovery.retry().await;
                retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
            }
        }
    }
}

async fn converge_startup_managed_install_durable_outcome(
    mut outcome: ManagedInstallDurableOutcome,
    request_drain: &mut InstallRequestDrain,
) -> Option<ManagedInstallDurableOutcome> {
    let mut retry_delay = Duration::from_millis(100);
    for attempt in 0..STARTUP_PUBLICATION_SETTLEMENT_ATTEMPTS {
        match outcome {
            ManagedInstallDurableOutcome::Indeterminate(recovery) => {
                if attempt + 1 == STARTUP_PUBLICATION_SETTLEMENT_ATTEMPTS
                    || !wait_for_managed_publication_retry(request_drain, retry_delay).await
                {
                    return None;
                }
                outcome = recovery.retry().await;
                retry_delay = retry_delay.saturating_mul(2);
            }
            settled => return Some(settled),
        }
    }
    None
}

async fn converge_startup_managed_install_acknowledgement(
    mut outcome: ManagedInstallAcknowledgementOutcome,
    request_drain: &mut InstallRequestDrain,
) -> bool {
    let mut retry_delay = Duration::from_millis(100);
    for attempt in 0..STARTUP_PUBLICATION_SETTLEMENT_ATTEMPTS {
        match outcome {
            ManagedInstallAcknowledgementOutcome::Acknowledged => return true,
            ManagedInstallAcknowledgementOutcome::Indeterminate(recovery) => {
                if attempt + 1 == STARTUP_PUBLICATION_SETTLEMENT_ATTEMPTS
                    || !wait_for_managed_publication_retry(request_drain, retry_delay).await
                {
                    return false;
                }
                outcome = recovery.retry().await;
                retry_delay = retry_delay.saturating_mul(2);
            }
        }
    }
    false
}

async fn acknowledge_startup_managed_install_publication<Acknowledgement>(
    acknowledgement: Acknowledgement,
    request_drain: &mut InstallRequestDrain,
) -> bool
where
    Acknowledgement: Future<Output = ManagedInstallAcknowledgementOutcome>,
{
    tokio::time::timeout(STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT, async {
        let outcome = acknowledgement.await;
        converge_startup_managed_install_acknowledgement(outcome, request_drain).await
    })
    .await
    .unwrap_or(false)
}

async fn settle_managed_install_publication(
    managed_root: ManagedLibraryOperation,
    expected_version_id: &str,
    committed_kind: operation::InstallPublicationCheckpointKind,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    progress_tx: &InstallProgressSender,
    request_drain: &mut InstallRequestDrain,
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    receipt: axial_minecraft::KnownGoodInstallReceipt,
) -> DurableInstallPublication {
    if !publish_install_progress_durably(progress_tx, publication_indeterminate_install_progress())
        .await
    {
        return DurableInstallPublication::DeferredNonterminal;
    }
    let outcome =
        classify_managed_install_publication(managed_root, expected_version_id.to_string()).await;
    let Some(outcome) = converge_managed_install_durable_outcome(outcome, request_drain).await
    else {
        return DurableInstallPublication::DeferredNonterminal;
    };
    match outcome {
        ManagedInstallDurableOutcome::Mismatch => {
            DurableInstallPublication::Refused(managed_install_publication_mismatch_error())
        }
        ManagedInstallDurableOutcome::Committed(evidence)
            if evidence.id().matches_version_id(expected_version_id)
                && receipt.version_id() == expected_version_id =>
        {
            let evidence_id = evidence.id().clone();
            let verified = match evidence.verify_install_receipt(receipt) {
                Ok(verified) => verified,
                Err(_) => return DurableInstallPublication::DeferredNonterminal,
            };
            let checkpoint = operation::InstallPublicationCheckpoint {
                kind: committed_kind,
                version_id: expected_version_id.to_string(),
                evidence: evidence_id,
                activation_contract_id: Some(verified.activation_contract_id().clone()),
            };
            if operation::record_install_publication_checkpoint(journals, operation_id, &checkpoint)
                .await
                .is_err()
            {
                return DurableInstallPublication::DeferredNonterminal;
            }
            let acknowledgement = match state
                .accept_verified_known_good_install_receipt(foreground, library_operation, verified)
                .await
            {
                Ok(acknowledgement) => acknowledgement,
                Err(_) => return DurableInstallPublication::DeferredNonterminal,
            };
            if !converge_managed_install_acknowledgement(
                acknowledgement.acknowledge().await,
                request_drain,
            )
            .await
            {
                return DurableInstallPublication::DeferredNonterminal;
            }
            DurableInstallPublication::Committed
        }
        ManagedInstallDurableOutcome::RolledBack {
            evidence, effect, ..
        } if evidence.id().matches_version_id(expected_version_id) => {
            tracing::warn!(
                operation_id = %operation_id,
                expected_version_id,
                rollback_effect = ?effect,
                "managed install publication rolled back durably"
            );
            let checkpoint = operation::InstallPublicationCheckpoint {
                kind: operation::InstallPublicationCheckpointKind::RolledBack,
                version_id: expected_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            if operation::record_install_publication_checkpoint(journals, operation_id, &checkpoint)
                .await
                .is_err()
                || !converge_managed_install_acknowledgement(
                    evidence.acknowledge().await,
                    request_drain,
                )
                .await
            {
                return DurableInstallPublication::DeferredNonterminal;
            }
            DurableInstallPublication::RolledBack(DownloadError::FileOperation(
                std::io::Error::other("managed install publication was rolled back"),
            ))
        }
        ManagedInstallDurableOutcome::Committed(_)
        | ManagedInstallDurableOutcome::RolledBack { .. }
        | ManagedInstallDurableOutcome::NoEffect
        | ManagedInstallDurableOutcome::Indeterminate(_) => {
            DurableInstallPublication::DeferredNonterminal
        }
    }
}

enum InstallProgressCommand {
    Publish(DownloadProgress),
    PublishDurably {
        progress: DownloadProgress,
        acknowledgement: oneshot::Sender<bool>,
    },
}

impl InstallProgressCommand {
    fn into_parts(self) -> (DownloadProgress, Option<oneshot::Sender<bool>>, bool) {
        match self {
            Self::Publish(progress) => (progress, None, false),
            Self::PublishDurably {
                progress,
                acknowledgement,
            } => (progress, Some(acknowledgement), true),
        }
    }

    #[cfg(test)]
    fn into_progress(self) -> DownloadProgress {
        self.into_parts().0
    }
}

type InstallProgressSender = mpsc::UnboundedSender<InstallProgressCommand>;

fn publish_install_progress(
    progress_tx: &InstallProgressSender,
    progress: DownloadProgress,
) -> bool {
    progress_tx
        .send(InstallProgressCommand::Publish(progress))
        .is_ok()
}

async fn publish_install_progress_durably(
    progress_tx: &InstallProgressSender,
    progress: DownloadProgress,
) -> bool {
    let (acknowledgement, committed) = oneshot::channel();
    if progress_tx
        .send(InstallProgressCommand::PublishDurably {
            progress,
            acknowledgement,
        })
        .is_err()
    {
        return false;
    }
    committed.await.unwrap_or(false)
}

pub(crate) async fn settle_startup_install_guardian_failure_memory(state: &AppState) -> bool {
    let Ok(producer) = state.try_claim_producer() else {
        return false;
    };
    let journals = state.journals().clone();
    let failure_memory = state.failure_memory().clone();
    match producer
        .spawn_joinable(async move {
            operation::settle_startup_install_guardian_failure_memory(&journals, &failure_memory)
                .await
        })
        .await
    {
        Ok(Ok(())) => true,
        Ok(Err(error)) => {
            tracing::error!(
                error_class = error.class(),
                "Guardian install startup failure-memory barrier remains unsettled"
            );
            false
        }
        Err(error) => {
            let join_failure_kind = if error.is_panic() {
                "panic"
            } else if error.is_cancelled() {
                "cancelled"
            } else {
                "unknown"
            };
            tracing::error!(
                join_failure_kind,
                "Guardian install startup failure-memory task stopped unexpectedly"
            );
            false
        }
    }
}

struct RecoveringInstallAdmission {
    journal: operation::RecoveringInstallJournal,
    foreground: InstallForegroundActivity,
}

fn signal_startup_install_settled(sender: &mut Option<oneshot::Sender<()>>) -> Result<(), ()> {
    sender.take().ok_or(())?.send(()).map_err(|_| ())
}

async fn await_startup_install_barriers(
    startup_barriers: Vec<oneshot::Receiver<()>>,
    deadline: Duration,
) -> bool {
    tokio::time::timeout(deadline, async move {
        for startup_barrier in startup_barriers {
            if startup_barrier.await.is_err() {
                return false;
            }
        }
        true
    })
    .await
    .unwrap_or(false)
}

pub(super) fn checkpoint_recovery_blocks_explicit_known_good_rebuild(
    journals: &OperationJournalStore,
) -> bool {
    let Ok(journals) = operation::recovering_install_journals(journals) else {
        return true;
    };
    journals.iter().any(|journal| {
        journal.checkpoints.iter().any(|checkpoint| {
            matches!(
                checkpoint.kind,
                operation::InstallPublicationCheckpointKind::Committed
                    | operation::InstallPublicationCheckpointKind::BaseCommitted
                    | operation::InstallPublicationCheckpointKind::ChildCommitted
            )
        })
    })
}

pub(crate) async fn rehydrate_startup_installs(state: &AppState) -> bool {
    rehydrate_startup_installs_with_checkpoint_reconstruction(state, |version_id| async move {
        axial_minecraft::reconstruct_known_good(&version_id).await
    })
    .await
}

async fn rehydrate_startup_installs_with_checkpoint_reconstruction<Reconstruct, Reconstruction>(
    state: &AppState,
    reconstruct: Reconstruct,
) -> bool
where
    Reconstruct: Fn(String) -> Reconstruction + Clone + Send + Sync + 'static,
    Reconstruction: Future<
            Output = Result<
                axial_minecraft::KnownGoodReconstructionReceipt,
                axial_minecraft::KnownGoodReconstructionError,
            >,
        > + Send
        + 'static,
{
    let journals = match operation::recovering_install_journals(state.journals()) {
        Ok(journals) => journals,
        Err(error) => {
            tracing::error!(
                error_class = ?error,
                "startup install journal validation failed"
            );
            return false;
        }
    };
    if journals.is_empty() {
        return true;
    }
    let Ok(producer) = state.try_claim_producer() else {
        return false;
    };
    let mut admissions = Vec::with_capacity(journals.len());
    for journal in journals {
        let update_admission = match state.try_admit_update_sensitive_operation() {
            Ok(admission) => admission,
            Err(_) => {
                rollback_recovering_install_admissions(state, &admissions).await;
                return false;
            }
        };
        let registration = match state.register_integrity_foreground() {
            Ok(registration) => registration,
            Err(_) => {
                rollback_recovering_install_admissions(state, &admissions).await;
                return false;
            }
        };
        let foreground = registration.wait_for_settlement().await;
        let admitted = match &journal.identity {
            operation::InstallJournalIdentity::Vanilla { version_id } => {
                state
                    .installs()
                    .admit_recovering_vanilla(
                        journal.install_id.clone(),
                        journal.operation_id.clone(),
                        version_id.clone(),
                    )
                    .await
            }
            operation::InstallJournalIdentity::Loader {
                component_id,
                build_id,
                ..
            } => {
                state
                    .installs()
                    .admit_recovering_loader(
                        journal.install_id.clone(),
                        journal.operation_id.clone(),
                        *component_id,
                        build_id.clone(),
                    )
                    .await
            }
        };
        if admitted.is_err() {
            rollback_recovering_install_admissions(state, &admissions).await;
            return false;
        }
        admissions.push(RecoveringInstallAdmission {
            journal,
            foreground: InstallForegroundActivity::new_with_update_admission(
                foreground,
                update_admission,
            ),
        });
    }
    let mut startup_barriers = Vec::with_capacity(admissions.len());
    for admission in admissions {
        let recovering_install_id = admission.journal.install_id.clone();
        let (startup_settled, startup_barrier) = oneshot::channel();
        state
            .installs()
            .emit(
                &admission.journal.install_id,
                publication_indeterminate_install_progress(),
            )
            .await;
        match &admission.journal.identity {
            operation::InstallJournalIdentity::Vanilla { .. } => {
                spawn_recovering_vanilla_install_with_checkpoint_reconstruction(
                    state.clone(),
                    producer.claim_child(),
                    admission,
                    startup_settled,
                    reconstruct.clone(),
                );
            }
            operation::InstallJournalIdentity::Loader { .. } => {
                loader::spawn_recovering_loader_install(
                    state.clone(),
                    producer.claim_child(),
                    admission,
                    startup_settled,
                    reconstruct.clone(),
                );
            }
        }
        spawn_recovered_install_queue_wake_owned(
            state.clone(),
            recovering_install_id,
            producer.claim_child(),
        );
        startup_barriers.push(startup_barrier);
    }
    await_startup_install_barriers(startup_barriers, STARTUP_INSTALL_BARRIER_TIMEOUT).await
}

async fn rollback_recovering_install_admissions(
    state: &AppState,
    admissions: &[RecoveringInstallAdmission],
) {
    for admission in admissions {
        state.installs().remove(&admission.journal.install_id).await;
    }
}

pub(crate) const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";
pub(crate) const LOADER_INSTALL_INTERRUPTED_MESSAGE: &str =
    "Loader install stopped before completing. Try again.";
pub(crate) const BASE_INSTALL_FAILED_MESSAGE: &str =
    "Base game install failed. Retry the install from Downloads.";
const INSTALL_JOURNAL_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(10);
const INSTALL_JOURNAL_RETRY_MAX_DELAY: Duration = Duration::from_secs(1);
const STARTUP_PUBLICATION_SETTLEMENT_ATTEMPTS: usize = 4;
const STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT: Duration = Duration::from_secs(5);
const STARTUP_INSTALL_BARRIER_TIMEOUT: Duration = Duration::from_secs(30);
const OPERATION_ID_RESERVATION_ATTEMPTS: usize = 8;
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
    admission: InstallAdmissionMarker,
    cleanup_owner: Option<ProducerLease>,
    foreground: Option<IntegrityForegroundLease>,
}

struct ContentInitializationReservation {
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    install_id: Option<String>,
    operation_id: OperationId,
    expected_plan: OperationJournalEntry,
    admission: InstallAdmissionMarker,
    cleanup_owner: Option<ProducerLease>,
}

impl ContentInitializationReservation {
    fn new(
        store: Arc<InstallStore>,
        journals: Arc<OperationJournalStore>,
        install_id: String,
        operation_id: OperationId,
        expected_plan: OperationJournalEntry,
        admission: InstallAdmissionMarker,
        cleanup_owner: ProducerLease,
    ) -> Self {
        Self {
            store,
            journals,
            install_id: Some(install_id),
            operation_id,
            expected_plan,
            admission,
            cleanup_owner: Some(cleanup_owner),
        }
    }

    fn hand_off(mut self) {
        self.install_id = None;
    }
}

impl Drop for ContentInitializationReservation {
    fn drop(&mut self) {
        if !self.admission.is_admitted() {
            return;
        }
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
        admission: InstallAdmissionMarker,
        cleanup_owner: ProducerLease,
        foreground: IntegrityForegroundLease,
    ) -> Self {
        Self {
            store,
            journals,
            install_id: Some(install_id),
            operation_id,
            admission,
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

    fn retained_foreground(&self) -> IntegrityForegroundLease {
        self.foreground
            .as_ref()
            .expect("install initialization foreground owner remains available")
            .retained()
    }
}

impl Drop for InstallInitializationReservation {
    fn drop(&mut self) {
        if !self.admission.is_admitted() {
            return;
        }
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
    reservation: InstallInitializationReservation,
    identity: operation::InstallJournalIdentity,
    producer: &ProducerLease,
) -> Result<InstallInitializationReservation, ()> {
    let journals = reservation.journals.clone();
    let operation_id = reservation.operation_id.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.claim_child().spawn(async move {
        match operation::begin_install_operation_journal_for_session(
            &journals,
            &operation_id,
            reservation.install_id.as_deref().unwrap_or_default(),
            &identity,
        )
        .await
        {
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
                let install_id = reservation.install_id.as_deref().unwrap_or_default();
                let expected = operation::planned_install_journal_for_session(
                    &operation_id,
                    install_id,
                    &identity,
                );
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
                    match operation::begin_install_operation_journal_for_session(
                        &journals,
                        &operation_id,
                        install_id,
                        &identity,
                    )
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
    reservation: ContentInitializationReservation,
    instance_id: String,
    producer: &ProducerLease,
) -> Result<ContentInitializationReservation, ()> {
    let journals = reservation.journals.clone();
    let operation_id = reservation.operation_id.clone();
    let expected = reservation.expected_plan.clone();
    let (result_tx, result_rx) = tokio::sync::oneshot::channel();
    producer.claim_child().spawn(async move {
        let mut result = operation::begin_content_operation_journal_for_session(
            &journals,
            &operation_id,
            reservation.install_id.as_deref().unwrap_or_default(),
            &instance_id,
        )
        .await;
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
                            result = operation::begin_content_operation_journal_for_session(
                                &journals,
                                &operation_id,
                                reservation.install_id.as_deref().unwrap_or_default(),
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

fn require_available_install_library(state: &AppState) -> Result<(), InstallApplicationError> {
    match state.managed_library_status().availability {
        ManagedLibraryAvailability::Ready { .. } => Ok(()),
        ManagedLibraryAvailability::Unconfigured => Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )),
        ManagedLibraryAvailability::Degraded(_) => Err((
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({
                "error": "Axial library is unavailable. Restore the configured folder and permissions, then restart Axial."
            })),
        )),
        ManagedLibraryAvailability::Changing { .. } => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "Axial library configuration is changing. Try again."
            })),
        )),
        ManagedLibraryAvailability::Closed => Err(install_shutdown_error_response()),
    }
}

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
    let request_drain = producer.wait_for_request_drain_start();
    producer.spawn(async move {
        tokio::select! {
            biased;
            _ = request_drain => {}
            _ = wait_for_install_terminal(&state, &install_id) => {}
        }
        drop(foreground);
    });
}

pub(crate) struct ContinuationInstallQueueResult {
    pub response: InstallQueueStateResponse,
    pub outcome: InstallQueueEnqueueOutcome,
}

impl ContinuationInstallQueueResult {
    pub(crate) fn queue_id(&self) -> &str {
        self.outcome.queue_id()
    }
}

struct InstallQueueSelection {
    notice: InstallQueueNoticeViewModel,
    outcome: InstallQueueEnqueueOutcome,
}

pub use loader::loader_pre_operation_error_response;
use loader::start_loader_install_with_foreground;
#[cfg(test)]
use loader::{
    LoaderInstallFailureRequest, dispatch_loader_install_failure, loader_install_done_progress,
    loader_install_error_progress, observe_active_vanilla_base_install,
    wait_for_observed_vanilla_base_install,
};
pub use model::{
    InstallActionViewModel, InstallFailureViewModel, InstallProgressStepViewModel,
    InstallProgressViewModel, InstallQueueActiveViewModel, InstallQueueContentActionRequest,
    InstallQueueContentItemViewModel, InstallQueueContentSelection,
    InstallQueueInstallItemViewModel, InstallQueueLoaderItemViewModel, InstallQueueNoticeViewModel,
    InstallQueueRequest, InstallQueueStateResponse, InstallQueueViewModel,
    InstallQueuedItemViewModel, InstallStartResponse, InstallStatusResponse,
    InstallVersionStartRequest, LoaderInstallStartRequest,
};
use operation::{
    ContentDownloadFactAccumulator, ContentFailureOutcomeRequest, InstallProgressCoalescer,
    InstallProgressPresenter, install_failure_evidence_from_download_error_or_facts,
    install_failure_evidence_from_download_facts, install_failure_point_from_journal,
    install_journal_is_terminal, install_progress_history_from_journal,
    install_progress_with_terminal_error, interrupted_install_progress,
    loader_install_progress_record_view_model, observed_install_failure_progress,
    public_install_id, publication_indeterminate_install_progress, record_content_failure_outcome,
    record_content_operation_interrupted, record_content_operation_progress,
    record_install_guardian_failure_outcome,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome,
    vanilla_install_progress_record_view_model, vanilla_install_progress_view_model,
};
pub use operation::{
    InstallProgressJournalTracker, install_guardian_outcome_summary_from_journal,
    public_loader_install_progress_record_json, public_vanilla_install_progress_record_json,
    reconcile_install_operation_terminal, record_install_operation_interrupted,
    record_install_operation_progress, sanitize_install_progress,
};
#[cfg(test)]
pub(crate) use operation::{begin_install_operation_journal, test_operation_id};
#[cfg(test)]
use operation::{loader_install_progress_view_model, typed_runtime_failure_evidence};
pub(crate) use stream::{install_events_stream, loader_install_events_stream};

async fn start_install_version_with_foreground(
    state: &AppState,
    request: InstallVersionStartRequest,
    producer: &ProducerLease,
    inherited_foreground: Option<IntegrityForegroundLease>,
    queue_start: &InstallQueueStartAuthority,
) -> Result<InstallStartResponse, InstallQueueStartFailure> {
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let version_id = effective_install_version_id(&request);
    if version_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        )
            .into());
    }
    require_available_install_library(state)?;

    let foreground = match inherited_foreground {
        Some(foreground) => foreground,
        None => {
            register_install_foreground(state)?
                .wait_for_settlement()
                .await
        }
    };

    let store = state.installs().clone();
    let journals = state.journals().clone();
    let mut admitted_install = None;
    for _ in 0..OPERATION_ID_RESERVATION_ATTEMPTS {
        let candidate = generate_install_id("install");
        if operation::install_operation_journal_for_session(state.journals(), &candidate).is_some()
        {
            continue;
        }
        let Some(candidate_operation_id) = mint_available_install_operation_id(state).await else {
            break;
        };
        let admission = InstallAdmissionMarker::new();
        let reservation = InstallInitializationReservation::new(
            store.clone(),
            journals.clone(),
            candidate.clone(),
            candidate_operation_id.clone(),
            admission.clone(),
            producer.claim_child(),
            foreground.retained(),
        );
        match store
            .admit_queued_vanilla(
                queue_start,
                &admission,
                candidate.clone(),
                candidate_operation_id.clone(),
                version_id.clone(),
            )
            .await
        {
            InstallQueueAdmission::Inserted => {
                admitted_install = Some((candidate, candidate_operation_id, reservation));
                break;
            }
            InstallQueueAdmission::Existing {
                install_id,
                operation_id,
            } => {
                drop(reservation);
                match store.wait_for_initialization(&install_id).await {
                    InstallInitializationStatus::Initialized => {
                        return Ok(InstallStartResponse {
                            operation_id,
                            install_id,
                            view_model: InstallProgressViewModel::starting(),
                        });
                    }
                    InstallInitializationStatus::Reconciling => {
                        return Err(install_journal_error_response().into());
                    }
                    InstallInitializationStatus::Removed => {
                        if !store
                            .reset_removed_queued_install_start(queue_start, &install_id)
                            .await
                        {
                            return Err(install_queue_start_stopped_error_response().into());
                        }
                    }
                }
            }
            InstallQueueAdmission::BlockedByLiveSession => {
                drop(reservation);
                return Err(InstallQueueStartFailure::BlockedByLiveSession);
            }
            InstallQueueAdmission::InstallIdCollision
            | InstallQueueAdmission::OperationIdCollision => {
                drop(reservation);
            }
            InstallQueueAdmission::ReservationLost => {
                drop(reservation);
                return Err(install_queue_start_stopped_error_response().into());
            }
        };
    }
    let Some((install_id, operation_id, reservation)) = admitted_install else {
        return Err(install_journal_error_response().into());
    };
    drop(foreground);
    let reservation = begin_install_journal_with_owned_reconciliation(
        reservation,
        operation::InstallJournalIdentity::vanilla(version_id.clone()),
        producer,
    )
    .await
    .map_err(|_| InstallQueueStartFailure::Application(install_journal_error_response()))?;
    if !store.mark_initialized(&install_id).await {
        return Err(install_journal_error_response().into());
    }

    let failure_memory = state.failure_memory().clone();
    let telemetry = state.telemetry().clone();
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_failure_memory = failure_memory.clone();
    let worker_operation_id = operation_id_task.clone();
    let reconciliation_telemetry = telemetry.clone();
    let worker_state = state.clone();
    let worker_runtime_cache = state.managed_runtime_cache().clone();
    let progress_owner = producer.claim_child();
    let guardian_owner = producer.claim_child();
    let foreground = InstallForegroundActivity::new_with_update_admission(
        reservation.retained_foreground(),
        update_admission,
    );
    let worker_initialization = reservation;
    let worker_foreground = foreground.clone();
    let interrupted_foreground = foreground.clone();
    let interrupted_state = state.clone();
    let reconciliation_foreground = foreground.clone();
    let reconciliation_state = state.clone();
    let reconciliation_journals = journals.clone();
    let reconciliation_operation_id = operation_id_task.clone();
    let worker_failure_journals = journals.clone();
    let worker_failure_operation_id = operation_id_task.clone();
    let worker_failure_state = state.clone();
    let worker_failure_foreground = foreground.clone();
    let worker_failure_install_id = install_id_task.clone();
    let worker_failure_request_drain = producer.wait_for_request_drain_start();
    let worker_failure_rebuild_owner = producer.claim_child();
    let recovery_request_drain = producer.wait_for_request_drain_start();
    spawn_install_foreground_retention(
        state.clone(),
        install_id_task.clone(),
        producer.claim_child(),
        foreground,
    );
    InstallStore::spawn_tracked_worker_with_exit_handlers_owned(
        store,
        producer.claim_child(),
        install_id_task,
        interrupted_install_progress(),
        async move {
            drop(worker_initialization.hand_off());
            let mut recovery_request_drain: InstallRequestDrain = Box::pin(recovery_request_drain);
            let (progress_tx, progress_rx) = mpsc::unbounded_channel::<InstallProgressCommand>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let terminal_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let committed = own_install_progress(
                        store,
                        journals,
                        operation_id,
                        install_id,
                        progress_rx,
                    )
                    .await;
                    if !committed {
                        journal_failed.notify_one();
                    }
                    committed
                })
            };

            let progress_tx_for_downloader = progress_tx.clone();
            let terminal_progress_for_downloader = Arc::clone(&terminal_progress);
            let mut install_facts = Vec::new();
            let settlement = match worker_state.admit_managed_artifact_mutation() {
                Ok(mutation) => match worker_state.try_acquire_managed_library() {
                    Ok(library_operation) => {
                        let downloader = Downloader::new(
                            library_operation.retained_core(),
                            worker_runtime_cache,
                        );
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
                                let _ =
                                    publish_install_progress(&progress_tx_for_downloader, progress);
                            },
                            |fact| install_facts.push(fact),
                        );
                        let (result, authority) = await_managed_install_settlement_retaining(
                            (mutation, library_operation),
                            install,
                            journal_failed.notified(),
                        )
                        .await;
                        (result, Some(authority))
                    }
                    Err(error) => {
                        drop(mutation);
                        (Err(DownloadError::FileOperation(error)), None)
                    }
                },
                Err(error) => (
                    Err(DownloadError::FileOperation(std::io::Error::other(error))),
                    None,
                ),
            };
            let (install_result, authority) = settlement;
            let attempt_terminal_progress = terminal_progress
                .lock()
                .ok()
                .and_then(|mut progress| progress.take());
            let (install_result, authority) = match install_result {
                Err(DownloadError::PublicationIndeterminate(recovery)) => {
                    tracing::warn!(
                        operation_id = %worker_operation_id,
                        version_id = version_id.as_str(),
                        failure_kind = "publication_indeterminate",
                        "install worker entered owned publication recovery"
                    );
                    let recovering_committed = publish_install_progress_durably(
                        &progress_tx,
                        publication_indeterminate_install_progress(),
                    )
                    .await;
                    match converge_managed_install_publication(
                        recovery,
                        &mut recovery_request_drain,
                        recovering_committed,
                    )
                    .await
                    {
                        ManagedPublicationConvergence::Settled(result) => (result, authority),
                        ManagedPublicationConvergence::DeferredNonterminal => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(store_task).await;
                            drop(authority);
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    }
                }
                result => (result, authority),
            };
            let (final_install_succeeded, final_terminal_progress) = match install_result {
                Ok(receipt) => {
                    let publication = match (worker_foreground.retained(), authority.as_ref()) {
                        (Some(foreground), Some((_, library_operation)))
                            if receipt.version_id() == version_id
                                && worker_state
                                    .validate_managed_library_operation(library_operation)
                                    .is_ok() =>
                        {
                            settle_managed_install_publication(
                                library_operation.retained_core(),
                                &version_id,
                                operation::InstallPublicationCheckpointKind::Committed,
                                worker_journals.as_ref(),
                                &worker_operation_id,
                                &progress_tx,
                                &mut recovery_request_drain,
                                &worker_state,
                                &foreground,
                                library_operation,
                                receipt,
                            )
                            .await
                        }
                        _ => DurableInstallPublication::DeferredNonterminal,
                    };
                    match publication {
                        DurableInstallPublication::Committed => (true, attempt_terminal_progress),
                        DurableInstallPublication::RolledBack(error)
                        | DurableInstallPublication::Refused(error) => {
                            record_install_failure_outcome_for_error(
                                &guardian_owner,
                                worker_journals.clone(),
                                worker_failure_memory.clone(),
                                &worker_operation_id,
                                &error,
                                &install_facts,
                            )
                            .await;
                            (
                                false,
                                Some(install_progress_with_terminal_error(
                                    terminal_failure_progress_or_default(attempt_terminal_progress),
                                    &error,
                                )),
                            )
                        }
                        DurableInstallPublication::DeferredNonterminal => {
                            tracing::warn!(
                                operation_id = %worker_operation_id,
                                version_id = version_id.as_str(),
                                failure_kind = "durable_publication_reconciliation",
                                "install worker left publication recovery nonterminal"
                            );
                            drop(progress_tx);
                            let _ = finish_install_progress_task(store_task).await;
                            drop(authority);
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    }
                }
                Err(install_error) => {
                    tracing::warn!(
                        operation_id = %worker_operation_id,
                        version_id = version_id.as_str(),
                        failure_kind = install_error_log_kind(&install_error),
                        "install worker observed failed install"
                    );
                    record_install_failure_outcome_for_error(
                        &guardian_owner,
                        worker_journals.clone(),
                        worker_failure_memory.clone(),
                        &worker_operation_id,
                        &install_error,
                        &install_facts,
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
            let exact_terminal = sanitize_install_progress(terminal_progress.clone());
            let _ = publish_install_progress(&progress_tx, terminal_progress);
            drop(progress_tx);
            let _ = finish_install_progress_task(store_task).await;
            drop(authority);
            InstallStore::worker_exit_reconcile_terminal(exact_terminal)
        },
        move |interrupted_progress| async move {
            let _foreground =
                retain_install_foreground(&interrupted_state, &interrupted_foreground).await;
            match reconcile_install_worker_interruption(
                journals.as_ref(),
                &operation_id_task,
                interrupted_progress,
            )
            .await
            {
                Ok(progress) => Some(progress),
                Err(_) => {
                    tracing::warn!("failed to reconcile interrupted install journal");
                    None
                }
            }
        },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&reconciliation_state, &reconciliation_foreground).await;
            let progress = sanitize_install_progress(progress);
            let progress = match reconcile_install_operation_terminal(
                reconciliation_journals.as_ref(),
                &reconciliation_operation_id,
                &progress,
            )
            .await
            {
                Ok(progress) => progress,
                Err(_) => {
                    tracing::warn!("failed to reconcile exact install terminal");
                    return None;
                }
            };
            if let Some(summary) = progress.error.as_deref() {
                emit_install_failed(reconciliation_telemetry.as_ref(), summary);
            }
            Some(progress)
        },
        move || async move {
            let mut request_drain: InstallRequestDrain = Box::pin(worker_failure_request_drain);
            recover_vanilla_install_after_worker_failure(
                &worker_failure_state,
                &worker_failure_foreground,
                &worker_failure_rebuild_owner,
                worker_failure_journals.as_ref(),
                &worker_failure_operation_id,
                &worker_failure_install_id,
                &mut request_drain,
            )
            .await
        },
    );

    Ok(InstallStartResponse {
        install_id,
        operation_id,
        view_model: InstallProgressViewModel::starting(),
    })
}

enum RecoveringVanillaPublication {
    Fresh,
    Reconstruct {
        evidence: Option<ManagedInstallCommittedEvidence>,
        checkpoint: operation::InstallPublicationCheckpoint,
        checkpoint_recorded: bool,
    },
    RolledBack(Option<ManagedInstallRolledBackEvidence>),
    Refused(DownloadError),
    DeferredNonterminal,
}

enum RecoveringVanillaAuthority {
    #[cfg(test)]
    Installed(axial_minecraft::KnownGoodInstallReceipt),
    Reconstructed(axial_minecraft::KnownGoodReconstructionReceipt),
}

impl RecoveringVanillaAuthority {
    fn version_id(&self) -> &str {
        match self {
            #[cfg(test)]
            Self::Installed(receipt) => receipt.version_id(),
            Self::Reconstructed(receipt) => receipt.version_id(),
        }
    }
}

#[derive(Clone, Copy)]
pub(super) enum RecoveringInstallConvergence {
    StartupBounded,
    SameProcess,
}

async fn classify_recovering_vanilla_publication(
    managed_root: ManagedLibraryOperation,
    journal: &operation::RecoveringInstallJournal,
    request_drain: &mut InstallRequestDrain,
    journals: &OperationJournalStore,
    convergence: RecoveringInstallConvergence,
) -> RecoveringVanillaPublication {
    let expected_version_id = journal.identity.target_version_id();
    let verification_root = managed_root.clone();
    let outcome = match convergence {
        RecoveringInstallConvergence::StartupBounded => {
            tokio::time::timeout(STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT, async {
                let outcome = classify_managed_install_publication(
                    managed_root,
                    expected_version_id.to_string(),
                )
                .await;
                converge_startup_managed_install_durable_outcome(outcome, request_drain).await
            })
            .await
            .ok()
            .flatten()
        }
        RecoveringInstallConvergence::SameProcess => {
            let outcome =
                classify_managed_install_publication(managed_root, expected_version_id.to_string())
                    .await;
            converge_managed_install_durable_outcome(outcome, request_drain).await
        }
    };
    let Some(outcome) = outcome else {
        return RecoveringVanillaPublication::DeferredNonterminal;
    };
    let checkpoint = journal.checkpoints.first();
    match (checkpoint, outcome) {
        (_, ManagedInstallDurableOutcome::Mismatch) => {
            RecoveringVanillaPublication::Refused(managed_install_publication_mismatch_error())
        }
        (None, ManagedInstallDurableOutcome::NoEffect) => RecoveringVanillaPublication::Fresh,
        (None, ManagedInstallDurableOutcome::Committed(evidence))
            if evidence.id().matches_version_id(expected_version_id) =>
        {
            let Some(activation_contract_id) = evidence.committed_activation_contract_id().cloned()
            else {
                return RecoveringVanillaPublication::DeferredNonterminal;
            };
            RecoveringVanillaPublication::Reconstruct {
                checkpoint: operation::InstallPublicationCheckpoint {
                    kind: operation::InstallPublicationCheckpointKind::Committed,
                    version_id: expected_version_id.to_string(),
                    evidence: evidence.id().clone(),
                    activation_contract_id: Some(activation_contract_id),
                },
                evidence: Some(evidence),
                checkpoint_recorded: false,
            }
        }
        (
            None,
            ManagedInstallDurableOutcome::RolledBack {
                evidence, effect, ..
            },
        ) if evidence.id().matches_version_id(expected_version_id) => {
            tracing::warn!(
                operation_id = %journal.operation_id,
                rollback_effect = ?effect,
                "startup install recovery found a durable rollback"
            );
            let checkpoint = operation::InstallPublicationCheckpoint {
                kind: operation::InstallPublicationCheckpointKind::RolledBack,
                version_id: expected_version_id.to_string(),
                evidence: evidence.id().clone(),
                activation_contract_id: None,
            };
            if operation::record_install_publication_checkpoint(
                journals,
                &journal.operation_id,
                &checkpoint,
            )
            .await
            .is_ok()
            {
                RecoveringVanillaPublication::RolledBack(Some(evidence))
            } else {
                RecoveringVanillaPublication::DeferredNonterminal
            }
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::Committed(evidence))
            if checkpoint.kind == operation::InstallPublicationCheckpointKind::Committed
                && evidence.id().matches_version_id(expected_version_id)
                && evidence.id() == &checkpoint.evidence
                && evidence.committed_activation_contract_id()
                    == checkpoint.activation_contract_id.as_ref() =>
        {
            RecoveringVanillaPublication::Reconstruct {
                evidence: Some(evidence),
                checkpoint: checkpoint.clone(),
                checkpoint_recorded: true,
            }
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::RolledBack { evidence, .. })
            if checkpoint.kind == operation::InstallPublicationCheckpointKind::RolledBack
                && evidence.id().matches_version_id(expected_version_id)
                && evidence.id() == &checkpoint.evidence =>
        {
            RecoveringVanillaPublication::RolledBack(Some(evidence))
        }
        (Some(checkpoint), ManagedInstallDurableOutcome::NoEffect)
            if checkpoint.version_id == expected_version_id
                && verify_managed_install_publication_evidence_root(
                    &verification_root,
                    &checkpoint.evidence,
                ) =>
        {
            match checkpoint.kind {
                operation::InstallPublicationCheckpointKind::Committed => {
                    RecoveringVanillaPublication::Reconstruct {
                        evidence: None,
                        checkpoint: checkpoint.clone(),
                        checkpoint_recorded: true,
                    }
                }
                operation::InstallPublicationCheckpointKind::RolledBack => {
                    RecoveringVanillaPublication::RolledBack(None)
                }
                operation::InstallPublicationCheckpointKind::BaseCommitted
                | operation::InstallPublicationCheckpointKind::ChildCommitted => {
                    RecoveringVanillaPublication::DeferredNonterminal
                }
            }
        }
        _ => RecoveringVanillaPublication::DeferredNonterminal,
    }
}

async fn activate_recovering_vanilla_authority(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    library_operation: &crate::state::LibraryOperation,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    expected_version_id: &str,
    evidence: Option<ManagedInstallCommittedEvidence>,
    checkpoint: operation::InstallPublicationCheckpoint,
    checkpoint_recorded: bool,
    authority: RecoveringVanillaAuthority,
) -> Result<Option<axial_minecraft::ManagedInstallPostActivationAcknowledgement>, ()> {
    if checkpoint.kind != operation::InstallPublicationCheckpointKind::Committed
        || checkpoint.version_id != expected_version_id
        || authority.version_id() != expected_version_id
    {
        return Err(());
    }
    let expected_contract = checkpoint.activation_contract_id.as_ref().ok_or(())?;
    match evidence {
        Some(evidence)
            if evidence.id() == &checkpoint.evidence
                && evidence.committed_activation_contract_id() == Some(expected_contract) =>
        {
            let activation_contract_id = expected_contract.clone();
            let acknowledgement = match authority {
                #[cfg(test)]
                RecoveringVanillaAuthority::Installed(receipt) => {
                    let verified = evidence.verify_install_receipt(receipt).map_err(|_| ())?;
                    if verified.activation_contract_id() != &activation_contract_id {
                        return Err(());
                    }
                    if !checkpoint_recorded {
                        operation::record_install_publication_checkpoint(
                            journals,
                            operation_id,
                            &checkpoint,
                        )
                        .await
                        .map_err(|_| ())?;
                    }
                    state
                        .accept_verified_known_good_install_receipt(
                            foreground,
                            library_operation,
                            verified,
                        )
                        .await
                        .map_err(|_| ())?
                }
                RecoveringVanillaAuthority::Reconstructed(receipt) => {
                    let verified = evidence
                        .verify_reconstruction_receipt(receipt)
                        .map_err(|_| ())?;
                    if verified.activation_contract_id() != &activation_contract_id {
                        return Err(());
                    }
                    if !checkpoint_recorded {
                        operation::record_install_publication_checkpoint(
                            journals,
                            operation_id,
                            &checkpoint,
                        )
                        .await
                        .map_err(|_| ())?;
                    }
                    state
                        .accept_verified_known_good_reconstruction_receipt(
                            foreground,
                            library_operation,
                            verified,
                        )
                        .await
                        .map_err(|_| ())?
                }
            };
            Ok(Some(acknowledgement))
        }
        None if checkpoint_recorded => {
            #[cfg(test)]
            let receipt = match authority {
                RecoveringVanillaAuthority::Installed(_) => return Err(()),
                RecoveringVanillaAuthority::Reconstructed(receipt) => receipt,
            };
            #[cfg(not(test))]
            let RecoveringVanillaAuthority::Reconstructed(receipt) = authority;
            let verified =
                verify_managed_install_reconstruction_checkpoint(expected_contract, receipt)
                    .map_err(|_| ())?;
            state
                .accept_verified_known_good_checkpoint(foreground, library_operation, verified)
                .await
                .map_err(|_| ())?;
            Ok(None)
        }
        _ => Err(()),
    }
}

async fn converge_recovering_authority<Authority, Reconstruct, Reconstruction, MatchesIdentity>(
    version_id: &str,
    reconstruct: &mut Reconstruct,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
    matches_identity: MatchesIdentity,
) -> Option<Authority>
where
    Reconstruct: FnMut(String) -> Reconstruction,
    Reconstruction: Future<Output = Option<Authority>>,
    MatchesIdentity: Fn(&Authority, &str) -> bool,
{
    let mut retry_delay = Duration::from_millis(100);
    let maximum_retry_delay = Duration::from_secs(5);
    let deadline = matches!(convergence, RecoveringInstallConvergence::StartupBounded)
        .then(|| tokio::time::Instant::now() + STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT);
    loop {
        let reconstruction = reconstruct(version_id.to_string());
        tokio::pin!(reconstruction);
        let authority = match deadline {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = request_drain.as_mut() => return None,
                    _ = tokio::time::sleep_until(deadline) => return None,
                    authority = &mut reconstruction => authority,
                }
            }
            None => {
                tokio::select! {
                    biased;
                    _ = request_drain.as_mut() => return None,
                    authority = &mut reconstruction => authority,
                }
            }
        };
        if let Some(authority) = authority
            && matches_identity(&authority, version_id)
        {
            return Some(authority);
        }
        match deadline {
            Some(deadline) => {
                tokio::select! {
                    biased;
                    _ = request_drain.as_mut() => return None,
                    _ = tokio::time::sleep_until(deadline) => return None,
                    _ = tokio::time::sleep(retry_delay) => {}
                }
            }
            None => {
                if !wait_for_managed_publication_retry(request_drain, retry_delay).await {
                    return None;
                }
            }
        }
        retry_delay = retry_delay.saturating_mul(2).min(maximum_retry_delay);
    }
}

async fn converge_worker_failure_reconstruction<Reconstruct, Reconstruction>(
    version_id: &str,
    reconstruct: &mut Reconstruct,
    request_drain: &mut InstallRequestDrain,
) -> Option<RecoveringVanillaAuthority>
where
    Reconstruct: FnMut(String) -> Reconstruction,
    Reconstruction: Future<Output = Option<RecoveringVanillaAuthority>>,
{
    converge_recovering_authority(
        version_id,
        reconstruct,
        request_drain,
        RecoveringInstallConvergence::SameProcess,
        |authority, expected| authority.version_id() == expected,
    )
    .await
}

pub(super) async fn converge_known_good_reconstruction(
    version_id: &str,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
) -> Option<axial_minecraft::KnownGoodReconstructionReceipt> {
    let mut reconstruct = |candidate: String| async move {
        axial_minecraft::reconstruct_known_good(&candidate).await
    };
    converge_known_good_reconstruction_with(
        version_id,
        &mut reconstruct,
        request_drain,
        convergence,
    )
    .await
}

pub(super) async fn converge_known_good_reconstruction_with<Reconstruct, Reconstruction>(
    version_id: &str,
    reconstruct: &mut Reconstruct,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
) -> Option<axial_minecraft::KnownGoodReconstructionReceipt>
where
    Reconstruct: FnMut(String) -> Reconstruction,
    Reconstruction: Future<
        Output = Result<
            axial_minecraft::KnownGoodReconstructionReceipt,
            axial_minecraft::KnownGoodReconstructionError,
        >,
    >,
{
    let mut source = |candidate: String| {
        let reconstruction = reconstruct(candidate);
        async move { reconstruction.await.ok() }
    };
    converge_recovering_authority(
        version_id,
        &mut source,
        request_drain,
        convergence,
        |receipt, expected| receipt.version_id() == expected,
    )
    .await
}

async fn rebuild_checkpoint_registered_known_good(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    library_operation: &crate::state::LibraryOperation,
    version_id: &str,
    checkpoint: &operation::InstallPublicationCheckpoint,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
) -> bool {
    rebuild_checkpoint_registered_known_good_with(
        state,
        foreground,
        producer,
        library_operation,
        version_id,
        checkpoint,
        |version_id| async move { axial_minecraft::reconstruct_known_good(&version_id).await },
        request_drain,
        convergence,
    )
    .await
}

async fn await_checkpoint_known_good_rebuild<Rebuild, Error>(
    rebuild: Rebuild,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
) -> bool
where
    Rebuild: Future<Output = Result<(), Error>>,
{
    tokio::pin!(rebuild);
    let settlement = async {
        tokio::select! {
            biased;
            _ = request_drain.as_mut() => false,
            result = &mut rebuild => result.is_ok(),
        }
    };
    match convergence {
        RecoveringInstallConvergence::StartupBounded => {
            tokio::time::timeout(STARTUP_PUBLICATION_SETTLEMENT_TIMEOUT, settlement)
                .await
                .unwrap_or(false)
        }
        RecoveringInstallConvergence::SameProcess => settlement.await,
    }
}

async fn rebuild_checkpoint_registered_known_good_with<Reconstruct, Reconstruction>(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    library_operation: &crate::state::LibraryOperation,
    version_id: &str,
    checkpoint: &operation::InstallPublicationCheckpoint,
    reconstruct: Reconstruct,
    request_drain: &mut InstallRequestDrain,
    convergence: RecoveringInstallConvergence,
) -> bool
where
    Reconstruct: FnOnce(String) -> Reconstruction + Send + 'static,
    Reconstruction: Future<
            Output = Result<
                axial_minecraft::KnownGoodReconstructionReceipt,
                axial_minecraft::KnownGoodReconstructionError,
            >,
        > + Send
        + 'static,
{
    let Some(expected_contract) = checkpoint.activation_contract_id.as_ref() else {
        return false;
    };
    await_checkpoint_known_good_rebuild(
        crate::application::known_good::rebuild_registered_known_good_for_version_with(
            state,
            foreground,
            producer,
            library_operation,
            version_id,
            expected_contract,
            reconstruct,
        ),
        request_drain,
        convergence,
    )
    .await
}

async fn recover_vanilla_install_after_worker_failure(
    state: &AppState,
    foreground: &InstallForegroundActivity,
    rebuild_owner: &ProducerLease,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    request_drain: &mut InstallRequestDrain,
) -> Option<DownloadProgress> {
    recover_vanilla_install_after_worker_failure_with_reconstruction(
        state,
        foreground,
        rebuild_owner,
        journals,
        operation_id,
        install_id,
        request_drain,
        |version_id| async move { axial_minecraft::reconstruct_known_good(&version_id).await },
        |version_id| async move {
            let receipt = axial_minecraft::reconstruct_known_good(&version_id)
                .await
                .ok()
                .filter(|receipt| receipt.version_id() == version_id)?;
            Some(RecoveringVanillaAuthority::Reconstructed(receipt))
        },
    )
    .await
}

async fn recover_vanilla_install_after_worker_failure_with_reconstruction<
    Rebuild,
    RebuildFuture,
    Reconstruct,
    Reconstruction,
>(
    state: &AppState,
    foreground: &InstallForegroundActivity,
    rebuild_owner: &ProducerLease,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    request_drain: &mut InstallRequestDrain,
    rebuild: Rebuild,
    mut reconstruct: Reconstruct,
) -> Option<DownloadProgress>
where
    Rebuild: FnOnce(String) -> RebuildFuture + Send + 'static,
    RebuildFuture: Future<
            Output = Result<
                axial_minecraft::KnownGoodReconstructionReceipt,
                axial_minecraft::KnownGoodReconstructionError,
            >,
        > + Send
        + 'static,
    Reconstruct: FnMut(String) -> Reconstruction,
    Reconstruction: Future<Output = Option<RecoveringVanillaAuthority>>,
{
    if let Some(progress) =
        operation::authoritative_install_terminal_progress(journals, operation_id)
    {
        return Some(progress);
    }
    let journal = match operation::recovering_install_journal(journals, operation_id) {
        Ok(journal)
            if journal.install_id == install_id
                && matches!(
                    &journal.identity,
                    operation::InstallJournalIdentity::Vanilla { .. }
                ) =>
        {
            journal
        }
        _ => return None,
    };
    if !record_worker_failure_recovery_marker(state, journals, operation_id, install_id).await {
        return None;
    }
    let version_id = journal.identity.target_version_id().to_string();
    let foreground = retain_install_foreground(state, foreground).await?;
    let Ok(mutation) = state.admit_managed_artifact_mutation() else {
        return None;
    };
    let Ok(library_operation) = state.try_acquire_managed_library() else {
        return None;
    };
    let publication = classify_recovering_vanilla_publication(
        library_operation.retained_core(),
        &journal,
        request_drain,
        journals,
        RecoveringInstallConvergence::SameProcess,
    )
    .await;
    let terminal = match publication {
        RecoveringVanillaPublication::Fresh => {
            drop(mutation);
            interrupted_install_progress()
        }
        RecoveringVanillaPublication::Reconstruct {
            evidence,
            checkpoint,
            checkpoint_recorded,
        } => {
            drop(mutation);
            if evidence.is_none() {
                if !rebuild_checkpoint_registered_known_good_with(
                    state,
                    &foreground,
                    rebuild_owner,
                    &library_operation,
                    &version_id,
                    &checkpoint,
                    rebuild,
                    request_drain,
                    RecoveringInstallConvergence::SameProcess,
                )
                .await
                {
                    return None;
                }
                vanilla_install_done_progress()
            } else {
                let authority = converge_worker_failure_reconstruction(
                    &version_id,
                    &mut reconstruct,
                    request_drain,
                )
                .await?;
                let mutation = state.admit_managed_artifact_mutation().ok()?;
                let acknowledgement = activate_recovering_vanilla_authority(
                    state,
                    &foreground,
                    &library_operation,
                    journals,
                    operation_id,
                    &version_id,
                    evidence,
                    checkpoint,
                    checkpoint_recorded,
                    authority,
                )
                .await
                .ok()?;
                drop(mutation);
                if let Some(acknowledgement) = acknowledgement
                    && !converge_managed_install_acknowledgement(
                        acknowledgement.acknowledge().await,
                        request_drain,
                    )
                    .await
                {
                    return None;
                }
                vanilla_install_done_progress()
            }
        }
        RecoveringVanillaPublication::RolledBack(evidence) => {
            drop(mutation);
            if let Some(evidence) = evidence
                && !converge_managed_install_acknowledgement(
                    evidence.acknowledge().await,
                    request_drain,
                )
                .await
            {
                return None;
            }
            let error = DownloadError::FileOperation(std::io::Error::other(
                "managed install publication was rolled back",
            ));
            install_progress_with_terminal_error(observed_install_failure_progress(), &error)
        }
        RecoveringVanillaPublication::Refused(error) => {
            drop(mutation);
            install_progress_with_terminal_error(observed_install_failure_progress(), &error)
        }
        RecoveringVanillaPublication::DeferredNonterminal => {
            drop(mutation);
            return None;
        }
    };
    drop(library_operation);
    Some(sanitize_install_progress(terminal))
}

pub(super) async fn record_worker_failure_recovery_marker(
    state: &AppState,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
) -> bool {
    let progress = publication_indeterminate_install_progress();
    if operation::record_install_operation_progress_durably(
        journals,
        operation_id,
        &progress,
        &mut InstallProgressJournalTracker::default(),
    )
    .await
    .is_err()
    {
        return false;
    }
    state.installs().emit(install_id, progress).await;
    true
}

fn spawn_recovering_vanilla_install_with_checkpoint_reconstruction<Reconstruct, Reconstruction>(
    state: AppState,
    producer: ProducerLease,
    admission: RecoveringInstallAdmission,
    startup_settled: oneshot::Sender<()>,
    reconstruct_checkpoint: Reconstruct,
) where
    Reconstruct: FnOnce(String) -> Reconstruction + Send + 'static,
    Reconstruction: Future<
            Output = Result<
                axial_minecraft::KnownGoodReconstructionReceipt,
                axial_minecraft::KnownGoodReconstructionError,
            >,
        > + Send
        + 'static,
{
    let RecoveringInstallAdmission {
        journal,
        foreground,
    } = admission;
    let operation::InstallJournalIdentity::Vanilla { version_id } = &journal.identity else {
        return;
    };
    let version_id = version_id.clone();
    let install_id = journal.install_id.clone();
    let operation_id = journal.operation_id.clone();
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let failure_memory = state.failure_memory().clone();
    let telemetry = state.telemetry().clone();
    let runtime_cache = state.managed_runtime_cache().clone();
    let worker_state = state.clone();
    let worker_store = store.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id.clone();
    let worker_install_id = install_id.clone();
    let worker_foreground = foreground.clone();
    let terminal_state = state.clone();
    let terminal_foreground = foreground.clone();
    let terminal_journals = journals.clone();
    let terminal_operation_id = operation_id.clone();
    let terminal_telemetry = telemetry.clone();
    let worker_failure_journals = journals.clone();
    let worker_failure_operation_id = operation_id.clone();
    let worker_failure_state = state.clone();
    let worker_failure_foreground = foreground.clone();
    let worker_failure_install_id = install_id.clone();
    let worker_failure_request_drain = producer.wait_for_request_drain_start();
    let checkpoint_rebuild_owner = producer.claim_child();
    let worker_failure_rebuild_owner = producer.claim_child();
    let progress_owner = producer.claim_child();
    let guardian_owner = producer.claim_child();
    let request_drain = producer.wait_for_request_drain_start();
    spawn_install_foreground_retention(
        state,
        install_id.clone(),
        producer.claim_child(),
        foreground,
    );
    InstallStore::spawn_tracked_worker_with_exit_handlers_owned(
        store,
        producer.claim_child(),
        install_id,
        interrupted_install_progress(),
        async move {
            let mut startup_settled = Some(startup_settled);
            let mut request_drain: InstallRequestDrain = Box::pin(request_drain);
            let (progress_tx, progress_rx) = mpsc::unbounded_channel::<InstallProgressCommand>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let progress_task = {
                let journal_failed = journal_failed.clone();
                let progress_journals = worker_journals.clone();
                let progress_operation_id = worker_operation_id.clone();
                progress_owner.spawn_joinable(async move {
                    let committed = own_install_progress(
                        worker_store,
                        progress_journals,
                        progress_operation_id,
                        worker_install_id,
                        progress_rx,
                    )
                    .await;
                    if !committed {
                        journal_failed.notify_one();
                    }
                    committed
                })
            };
            if !publish_install_progress_durably(
                &progress_tx,
                publication_indeterminate_install_progress(),
            )
            .await
            {
                drop(progress_tx);
                let _ = finish_install_progress_task(progress_task).await;
                return InstallStore::worker_exit_deferred_nonterminal();
            }
            let mutation = match worker_state.admit_managed_artifact_mutation() {
                Ok(mutation) => mutation,
                Err(_) => {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let library_operation = match worker_state.try_acquire_managed_library() {
                Ok(operation) => operation,
                Err(_) => {
                    drop(mutation);
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let publication = classify_recovering_vanilla_publication(
                library_operation.retained_core(),
                &journal,
                &mut request_drain,
                worker_journals.as_ref(),
                RecoveringInstallConvergence::StartupBounded,
            )
            .await;
            let terminal = match publication {
                RecoveringVanillaPublication::Fresh => {
                    if signal_startup_install_settled(&mut startup_settled).is_err() {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    let terminal_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
                    let terminal_progress_capture = terminal_progress.clone();
                    let progress_capture = progress_tx.clone();
                    let mut install_facts = Vec::new();
                    let downloader =
                        Downloader::new(library_operation.retained_core(), runtime_cache);
                    let install = downloader.install_version_with_facts(
                        &version_id,
                        move |progress| {
                            if progress.done {
                                if let Ok(mut terminal) = terminal_progress_capture.lock() {
                                    *terminal = Some(progress);
                                }
                            } else {
                                let _ = publish_install_progress(&progress_capture, progress);
                            }
                        },
                        |fact| install_facts.push(fact),
                    );
                    let (result, _) = await_managed_install_settlement_retaining(
                        (&mutation, &library_operation),
                        install,
                        journal_failed.notified(),
                    )
                    .await;
                    let result = match result {
                        Err(DownloadError::PublicationIndeterminate(recovery)) => {
                            match converge_managed_install_publication(
                                recovery,
                                &mut request_drain,
                                true,
                            )
                            .await
                            {
                                ManagedPublicationConvergence::Settled(result) => result,
                                ManagedPublicationConvergence::DeferredNonterminal => {
                                    drop(progress_tx);
                                    let _ = finish_install_progress_task(progress_task).await;
                                    return InstallStore::worker_exit_deferred_nonterminal();
                                }
                            }
                        }
                        result => result,
                    };
                    match result {
                        Ok(receipt) if receipt.version_id() == version_id.as_str() => {
                            let Some(foreground) = worker_foreground.retained() else {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            };
                            match settle_managed_install_publication(
                                library_operation.retained_core(),
                                &version_id,
                                operation::InstallPublicationCheckpointKind::Committed,
                                worker_journals.as_ref(),
                                &journal.operation_id,
                                &progress_tx,
                                &mut request_drain,
                                &worker_state,
                                &foreground,
                                &library_operation,
                                receipt,
                            )
                            .await
                            {
                                DurableInstallPublication::Committed => terminal_progress
                                    .lock()
                                    .ok()
                                    .and_then(|mut progress| progress.take())
                                    .unwrap_or_else(vanilla_install_done_progress),
                                DurableInstallPublication::RolledBack(error)
                                | DurableInstallPublication::Refused(error) => {
                                    record_install_failure_outcome_for_error(
                                        &guardian_owner,
                                        worker_journals.clone(),
                                        failure_memory.clone(),
                                        &journal.operation_id,
                                        &error,
                                        &install_facts,
                                    )
                                    .await;
                                    install_progress_with_terminal_error(
                                        observed_install_failure_progress(),
                                        &error,
                                    )
                                }
                                DurableInstallPublication::DeferredNonterminal => {
                                    drop(progress_tx);
                                    let _ = finish_install_progress_task(progress_task).await;
                                    return InstallStore::worker_exit_deferred_nonterminal();
                                }
                            }
                        }
                        Ok(_) => {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                        Err(error) => {
                            record_install_failure_outcome_for_error(
                                &guardian_owner,
                                worker_journals.clone(),
                                failure_memory.clone(),
                                &journal.operation_id,
                                &error,
                                &install_facts,
                            )
                            .await;
                            install_progress_with_terminal_error(
                                observed_install_failure_progress(),
                                &error,
                            )
                        }
                    }
                }
                RecoveringVanillaPublication::Reconstruct {
                    evidence,
                    checkpoint,
                    checkpoint_recorded,
                } => {
                    if evidence.is_none()
                        && signal_startup_install_settled(&mut startup_settled).is_err()
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    drop(mutation);
                    let Some(foreground) = worker_foreground.retained() else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    };
                    if evidence.is_none() {
                        if !rebuild_checkpoint_registered_known_good_with(
                            &worker_state,
                            &foreground,
                            &checkpoint_rebuild_owner,
                            &library_operation,
                            &version_id,
                            &checkpoint,
                            reconstruct_checkpoint,
                            &mut request_drain,
                            RecoveringInstallConvergence::StartupBounded,
                        )
                        .await
                        {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    } else {
                        let receipt = match converge_known_good_reconstruction(
                            &version_id,
                            &mut request_drain,
                            RecoveringInstallConvergence::StartupBounded,
                        )
                        .await
                        {
                            Some(receipt) => receipt,
                            None => {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            }
                        };
                        let Ok(mutation) = worker_state.admit_managed_artifact_mutation() else {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        };
                        let acknowledgement = match activate_recovering_vanilla_authority(
                            &worker_state,
                            &foreground,
                            &library_operation,
                            worker_journals.as_ref(),
                            &journal.operation_id,
                            &version_id,
                            evidence,
                            checkpoint,
                            checkpoint_recorded,
                            RecoveringVanillaAuthority::Reconstructed(receipt),
                        )
                        .await
                        {
                            Ok(acknowledgement) => acknowledgement,
                            Err(()) => {
                                drop(progress_tx);
                                let _ = finish_install_progress_task(progress_task).await;
                                return InstallStore::worker_exit_deferred_nonterminal();
                            }
                        };
                        drop(mutation);
                        if let Some(acknowledgement) = acknowledgement
                            && !acknowledge_startup_managed_install_publication(
                                acknowledgement.acknowledge(),
                                &mut request_drain,
                            )
                            .await
                        {
                            drop(progress_tx);
                            let _ = finish_install_progress_task(progress_task).await;
                            return InstallStore::worker_exit_deferred_nonterminal();
                        }
                    }
                    if startup_settled.is_some() {
                        let _ = signal_startup_install_settled(&mut startup_settled);
                    }
                    vanilla_install_done_progress()
                }
                RecoveringVanillaPublication::RolledBack(evidence) => {
                    drop(mutation);
                    if let Some(evidence) = evidence
                        && !acknowledge_startup_managed_install_publication(
                            evidence.acknowledge(),
                            &mut request_drain,
                        )
                        .await
                    {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(progress_task).await;
                        return InstallStore::worker_exit_deferred_nonterminal();
                    }
                    let _ = signal_startup_install_settled(&mut startup_settled);
                    let error = DownloadError::FileOperation(std::io::Error::other(
                        "managed install publication was rolled back",
                    ));
                    record_install_failure_outcome_for_error(
                        &guardian_owner,
                        worker_journals.clone(),
                        failure_memory,
                        &journal.operation_id,
                        &error,
                        &[],
                    )
                    .await;
                    install_progress_with_terminal_error(
                        observed_install_failure_progress(),
                        &error,
                    )
                }
                RecoveringVanillaPublication::Refused(error) => {
                    drop(mutation);
                    let _ = signal_startup_install_settled(&mut startup_settled);
                    record_install_failure_outcome_for_error(
                        &guardian_owner,
                        worker_journals.clone(),
                        failure_memory,
                        &journal.operation_id,
                        &error,
                        &[],
                    )
                    .await;
                    install_progress_with_terminal_error(
                        observed_install_failure_progress(),
                        &error,
                    )
                }
                RecoveringVanillaPublication::DeferredNonterminal => {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(progress_task).await;
                    return InstallStore::worker_exit_deferred_nonterminal();
                }
            };
            let exact_terminal = sanitize_install_progress(terminal.clone());
            let _ = publish_install_progress(&progress_tx, terminal);
            drop(progress_tx);
            let _ = finish_install_progress_task(progress_task).await;
            drop(library_operation);
            InstallStore::worker_exit_reconcile_terminal(exact_terminal)
        },
        |_| async { None },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&terminal_state, &terminal_foreground).await;
            let progress = match reconcile_install_operation_terminal(
                terminal_journals.as_ref(),
                &terminal_operation_id,
                &sanitize_install_progress(progress),
            )
            .await
            {
                Ok(progress) => progress,
                Err(_) => return None,
            };
            if let Some(summary) = progress.error.as_deref() {
                emit_install_failed(terminal_telemetry.as_ref(), summary);
            }
            Some(progress)
        },
        move || async move {
            let mut request_drain: InstallRequestDrain = Box::pin(worker_failure_request_drain);
            recover_vanilla_install_after_worker_failure(
                &worker_failure_state,
                &worker_failure_foreground,
                &worker_failure_rebuild_owner,
                worker_failure_journals.as_ref(),
                &worker_failure_operation_id,
                &worker_failure_install_id,
                &mut request_drain,
            )
            .await
        },
    );
}

pub(super) async fn record_install_failure_outcome(
    producer: &ProducerLease,
    journals: Arc<crate::state::OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    operation_id: &crate::state::contracts::OperationId,
    install_facts: &[ExecutionDownloadFact],
) {
    let evidence = install_failure_evidence_from_download_facts(operation_id, install_facts);
    record_install_failure_evidence(producer, journals, failure_memory, operation_id, &evidence)
        .await;
}

pub(super) async fn record_install_failure_outcome_for_error(
    producer: &ProducerLease,
    journals: Arc<crate::state::OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    operation_id: &crate::state::contracts::OperationId,
    error: &DownloadError,
    install_facts: &[ExecutionDownloadFact],
) {
    let evidence =
        install_failure_evidence_from_download_error_or_facts(operation_id, error, install_facts);
    record_install_failure_evidence(producer, journals, failure_memory, operation_id, &evidence)
        .await;
}

fn install_error_log_kind(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::FileOperation(_) => "file_operation",
        DownloadError::ResolveManifest(_) => "resolve_manifest",
        DownloadError::Request(_) => "request",
        DownloadError::ParseVersion(_) => "parse_version",
        DownloadError::PrepareRuntime(_) => "prepare_runtime",
        DownloadError::RuntimeSource(failure) => match failure.kind() {
            axial_minecraft::RuntimeSourceFailureKind::Unavailable => "runtime_source_unavailable",
            axial_minecraft::RuntimeSourceFailureKind::MetadataInvalid => {
                "runtime_source_metadata_invalid"
            }
            axial_minecraft::RuntimeSourceFailureKind::IntegrityMismatch => {
                "runtime_source_integrity_mismatch"
            }
            axial_minecraft::RuntimeSourceFailureKind::PolicyRejected => {
                "runtime_source_policy_rejected"
            }
        },
        DownloadError::RuntimeUnavailableForPlatform { .. } => "runtime_unavailable_for_platform",
        DownloadError::RuntimeRosettaRequired { .. } => "runtime_rosetta_required",
        DownloadError::Integrity(_) => "integrity",
        DownloadError::PublicationIndeterminate(_) => "publication_indeterminate",
        DownloadError::LibraryPlan(_) => "library_plan",
    }
}

async fn record_install_failure_evidence(
    producer: &ProducerLease,
    journals: Arc<crate::state::OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    operation_id: &crate::state::contracts::OperationId,
    evidence: &[ExecutionFact],
) {
    if record_install_guardian_failure_outcome(
        producer,
        journals,
        failure_memory,
        operation_id,
        evidence,
        crate::state::contracts::OperationPhase::Downloading,
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
    durable: bool,
    progress_journal: &mut InstallProgressJournalTracker,
    presenter: &mut InstallProgressPresenter,
) -> bool {
    let journal_result = if durable {
        operation::record_install_operation_progress_durably(
            journals,
            operation_id,
            &progress,
            progress_journal,
        )
        .await
    } else {
        record_install_operation_progress(journals, operation_id, &progress, progress_journal).await
    };
    if journal_result.is_err() {
        tracing::warn!("failed to record install journal progress");
        return false;
    }
    store
        .emit_record(install_id, presenter.record(progress))
        .await;
    true
}

async fn own_install_progress(
    store: Arc<InstallStore>,
    journals: Arc<OperationJournalStore>,
    operation_id: OperationId,
    install_id: String,
    mut progress_rx: mpsc::UnboundedReceiver<InstallProgressCommand>,
) -> bool {
    let mut coalescer = InstallProgressCoalescer::default();
    let mut presenter = InstallProgressPresenter::default();
    let mut progress_journal =
        InstallProgressJournalTracker::from_install_journal(&journals, &operation_id);
    while let Some(command) = progress_rx.recv().await {
        let (progress, acknowledgement, durable) = command.into_parts();
        let progress = sanitize_install_progress(progress);
        let mut committed = true;
        for progress in coalescer.push(progress) {
            if !record_and_emit_install_progress(
                store.as_ref(),
                journals.as_ref(),
                &operation_id,
                &install_id,
                progress,
                durable,
                &mut progress_journal,
                &mut presenter,
            )
            .await
            {
                committed = false;
                break;
            }
        }
        if let Some(acknowledgement) = acknowledgement {
            let _ = acknowledgement.send(committed);
        }
        if !committed {
            return false;
        }
    }
    if let Some(progress) = coalescer.flush()
        && !record_and_emit_install_progress(
            store.as_ref(),
            journals.as_ref(),
            &operation_id,
            &install_id,
            progress,
            false,
            &mut progress_journal,
            &mut presenter,
        )
        .await
    {
        return false;
    }
    true
}

async fn reconcile_install_worker_interruption(
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    interrupted_progress: DownloadProgress,
) -> Result<DownloadProgress, OperationJournalStoreError> {
    if let Some(progress) =
        operation::authoritative_install_terminal_progress(journals, operation_id)
    {
        return Ok(progress);
    }
    record_install_operation_interrupted(journals, operation_id, &interrupted_progress).await?;
    Ok(interrupted_progress)
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
    let snapshot = state.installs().snapshot(id).await;
    let journal = match snapshot.as_ref() {
        Some(snapshot) => state.journals().get(&snapshot.operation_id),
        None => operation::install_operation_journal_for_session(state.journals(), id),
    };
    if snapshot.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        ));
    }
    let operation_id = snapshot
        .as_ref()
        .map(|snapshot| snapshot.operation_id.clone())
        .or_else(|| journal.as_ref().map(|entry| entry.operation_id.clone()))
        .expect("an install status has a live snapshot or durable journal");

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
    let loader_install = snapshot
        .as_ref()
        .is_some_and(|snapshot| snapshot.loader_install);
    let view_model = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.latest.as_ref())
        .map(|record| {
            if loader_install {
                loader_install_progress_record_view_model(record)
            } else {
                vanilla_install_progress_record_view_model(record)
            }
        })
        .or_else(|| progress.last().map(vanilla_install_progress_view_model))
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
    let producer = state
        .try_claim_request_producer(&handoff)
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
    let selected_queue_id = selection.outcome.queue_id().to_string();
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
        let start_producer = producer.claim_child();
        let start_foreground = foreground.retained();
        maybe_start_selected_queued_install_owned_with(
            state,
            &selected_queue_id,
            owns_selected_queue,
            &producer,
            foreground,
            move |spec, queue_start| {
                let state = start_state.clone();
                let attempt_owner = start_producer.claim_child();
                let foreground = start_foreground.retained();
                async move {
                    start_queued_install(
                        &state,
                        &spec,
                        &attempt_owner,
                        Some(foreground),
                        &queue_start,
                    )
                    .await
                }
            },
        )
        .await?
    };
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
    let producer = state
        .try_claim_request_producer(&handoff)
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

pub(crate) async fn remove_queued_install_owned(
    state: &AppState,
    queue_id: &str,
    handoff: RequestProducerHandoff,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(|_| install_shutdown_error_response())?;
    let cleanup_foreground = state
        .register_integrity_foreground()
        .map_err(|_| install_shutdown_error_response())?;
    let cleanup_owner = producer.claim_child();
    let owner_state = state.clone();
    let queue_id = queue_id.to_string();
    producer
        .spawn_joinable(async move {
            let cleanup_foreground = cleanup_foreground.wait_for_settlement().await;
            remove_queued_install(&owner_state, cleanup_owner, cleanup_foreground, &queue_id).await
        })
        .await
        .map_err(|_| install_queue_remove_stopped_error_response())?
}

async fn remove_queued_install(
    state: &AppState,
    cleanup_owner: ProducerLease,
    cleanup_foreground: IntegrityForegroundLease,
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
            cleanup_owner,
            cleanup_foreground,
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
        .filter(|baseline| state.setup_instance_matches_baseline(baseline))
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

/// Remove an untouched instance created solely for setup, but only while no
/// launch or content mutation can be using it. Any metadata or filesystem
/// difference is treated as user ownership and retains the instance.
pub(crate) async fn remove_pristine_setup_instance(
    state: &AppState,
    owner: ProducerLease,
    foreground: IntegrityForegroundLease,
    instance_id: &str,
    cleanup: &SetupInstanceCleanup,
) -> bool {
    let Ok(update_admission) = state.try_admit_update_sensitive_operation() else {
        return false;
    };
    remove_pristine_setup_instance_admitted(
        state,
        owner,
        foreground,
        instance_id,
        cleanup,
        &update_admission,
    )
    .await
}

pub(crate) async fn remove_pristine_setup_instance_admitted(
    state: &AppState,
    owner: ProducerLease,
    foreground: IntegrityForegroundLease,
    instance_id: &str,
    cleanup: &SetupInstanceCleanup,
    _update_admission: &UpdateOperationLease,
) -> bool {
    state
        .delete_pristine_setup_instance_with_owner(
            owner,
            foreground,
            instance_id.to_string(),
            cleanup.clone(),
        )
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
    let cleanup_foreground = state
        .register_integrity_foreground()
        .map_err(|_| install_shutdown_error_response())?
        .wait_for_settlement()
        .await;
    let selection = enqueue_install_request(
        state,
        request,
        placement,
        prerequisite_queue_id,
        setup_cleanup,
    )
    .await?;
    let selected_queue_id = selection.outcome.queue_id().to_string();
    let owns_selected_queue = matches!(
        &selection.outcome,
        InstallQueueEnqueueOutcome::Enqueued { .. }
            | InstallQueueEnqueueOutcome::MovedToFront { .. }
    );
    let start_state = state.clone();
    let start_producer = producer.claim_child();
    let started =
        maybe_start_selected_queued_install_owned_with(
            state,
            &selected_queue_id,
            owns_selected_queue,
            &producer,
            &cleanup_foreground,
            move |spec, queue_start| {
                let state = start_state.clone();
                let attempt_owner = start_producer.claim_child();
                async move {
                    start_queued_install(&state, &spec, &attempt_owner, None, &queue_start).await
                }
            },
        )
        .await?;
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
            require_available_install_library(state)?;
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
            require_available_install_library(state)?;
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
    let producer = state
        .try_claim_request_producer(&handoff)
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
        |state, spec, producer, queue_start| async move {
            start_queued_install(&state, &spec, &producer, None, &queue_start).await
        },
    )
    .await
}

#[derive(Clone, Default)]
struct InstallQueueStartSupervisorState {
    queue_id: Arc<Mutex<Option<String>>>,
}

impl InstallQueueStartSupervisorState {
    fn record_reserved(&self, queue_id: &str) {
        *self
            .queue_id
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(queue_id.to_string());
    }

    fn clear(&self) {
        self.queue_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }

    fn take(&self) -> Option<String> {
        self.queue_id
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
}

async fn reconcile_unsettled_queue_start(
    state: &AppState,
    supervisor_state: &InstallQueueStartSupervisorState,
) -> InstallQueueStartReconciliation {
    let Some(queue_id) = supervisor_state.take() else {
        return InstallQueueStartReconciliation::Settled;
    };
    state
        .installs()
        .reconcile_queued_install_start(&queue_id)
        .await
}

fn validate_queue_start_reconciliation(
    result: &mut Result<Option<InstallStartResponse>, InstallApplicationError>,
    reconciliation: &InstallQueueStartReconciliation,
) {
    let Ok(Some(started)) = result else {
        return;
    };
    let exact = matches!(
        reconciliation,
        InstallQueueStartReconciliation::Started { install_id }
            if install_id == &started.install_id
    );
    if !exact {
        tracing::warn!(
            failure_kind = "queue_start_identity_mismatch",
            "queued install start returned without exact Store association"
        );
        *result = Err(install_queue_start_stopped_error_response());
    }
}

async fn supervise_queue_start_transaction(
    state: AppState,
    supervisor_state: InstallQueueStartSupervisorState,
    transaction: tokio::task::JoinHandle<
        Result<Option<InstallStartResponse>, InstallApplicationError>,
    >,
    monitor_producer: ProducerLease,
    result_sender: oneshot::Sender<Result<Option<InstallStartResponse>, InstallApplicationError>>,
) {
    let mut result = match transaction.await {
        Ok(result) => result,
        Err(_) => Err(install_queue_start_stopped_error_response()),
    };
    let reconciliation = reconcile_unsettled_queue_start(&state, &supervisor_state).await;
    validate_queue_start_reconciliation(&mut result, &reconciliation);
    if let InstallQueueStartReconciliation::Started { install_id } = reconciliation {
        spawn_install_queue_monitor_owned(state, install_id, monitor_producer);
    }
    let _ = result_sender.send(result);
}

async fn maybe_start_next_queued_install_owned_with<Start, StartFuture>(
    state: &AppState,
    producer: &ProducerLease,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnOnce(AppState, InstallQueueSpec, ProducerLease, InstallQueueStartAuthority) -> StartFuture
        + Send
        + 'static,
    StartFuture:
        Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>> + Send + 'static,
{
    let cleanup_foreground = state
        .register_integrity_foreground()
        .map_err(|_| install_shutdown_error_response())?;
    let supervisor_state = InstallQueueStartSupervisorState::default();
    let transaction_state = state.clone();
    let transaction_producer = producer.claim_child();
    let transaction_owner = transaction_producer.claim_child();
    let transaction_supervisor_state = supervisor_state.clone();
    let transaction = transaction_owner.spawn_joinable(async move {
        let cleanup_foreground = cleanup_foreground.wait_for_settlement().await;
        start_next_queued_install_transaction_with(
            &transaction_state,
            &transaction_producer,
            &cleanup_foreground,
            &transaction_supervisor_state,
            start,
        )
        .await
    });
    let (result_sender, result_receiver) = oneshot::channel();
    let supervisor_owner = producer.claim_child();
    let monitor_producer = producer.claim_child();
    supervisor_owner.spawn(supervise_queue_start_transaction(
        state.clone(),
        supervisor_state,
        transaction,
        monitor_producer,
        result_sender,
    ));
    result_receiver
        .await
        .unwrap_or_else(|_| Err(install_queue_start_stopped_error_response()))
}

async fn start_next_queued_install_transaction_with<Start, StartFuture>(
    state: &AppState,
    producer: &ProducerLease,
    cleanup_foreground: &IntegrityForegroundLease,
    supervisor_state: &InstallQueueStartSupervisorState,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnOnce(
        AppState,
        InstallQueueSpec,
        ProducerLease,
        InstallQueueStartAuthority,
    ) -> StartFuture,
    StartFuture: Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>>,
{
    let queue_start = state.installs().acquire_queue_start_gate().await;
    start_next_queued_install_transaction_under_gate_with(
        state,
        producer,
        cleanup_foreground,
        queue_start,
        supervisor_state,
        start,
    )
    .await
}

async fn start_next_queued_install_transaction_under_gate_with<Start, StartFuture>(
    state: &AppState,
    producer: &ProducerLease,
    cleanup_foreground: &IntegrityForegroundLease,
    queue_start: InstallQueueStartGuard,
    supervisor_state: &InstallQueueStartSupervisorState,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnOnce(
        AppState,
        InstallQueueSpec,
        ProducerLease,
        InstallQueueStartAuthority,
    ) -> StartFuture,
    StartFuture: Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>>,
{
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let entry = loop {
        let entry = match state.installs().reserve_next_queued_install().await {
            crate::state::InstallQueueReservation::Reserved(entry) => {
                supervisor_state.record_reserved(&entry.queue_id);
                entry
            }
            crate::state::InstallQueueReservation::Empty
            | crate::state::InstallQueueReservation::BlockedByLiveSession => {
                supervisor_state.clear();
                return Ok(None);
            }
        };
        if settle_unmet_queue_prerequisite(
            state,
            producer,
            cleanup_foreground,
            &entry,
            &update_admission,
        )
        .await
        {
            supervisor_state.clear();
            continue;
        }
        break entry;
    };
    let authority = queue_start.authorize(&entry.queue_id);
    let started = match start(
        state.clone(),
        entry.spec.clone(),
        producer.claim_child(),
        authority.clone(),
    )
    .await
    {
        Ok(started) => started,
        Err(InstallQueueStartFailure::BlockedByLiveSession) => {
            if !state
                .installs()
                .release_active_queued_install_to_front(&entry.queue_id)
                .await
            {
                return Err(install_queue_start_stopped_error_response());
            }
            supervisor_state.clear();
            return Ok(None);
        }
        Err(InstallQueueStartFailure::Application(error)) => {
            match state
                .installs()
                .settle_queued_install_start_failure(&authority)
                .await
            {
                InstallQueueStartFailureDisposition::CompletedUnlinked => {
                    supervisor_state.clear();
                }
                InstallQueueStartFailureDisposition::Started { .. } => {}
                InstallQueueStartFailureDisposition::Settled => {
                    supervisor_state.clear();
                    return Err(install_queue_start_stopped_error_response());
                }
            }
            return Err(error);
        }
    };
    if !state
        .installs()
        .queued_install_start_matches(&authority, &started.install_id)
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
    producer: &ProducerLease,
    cleanup_foreground: &IntegrityForegroundLease,
    start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnMut(InstallQueueSpec, InstallQueueStartAuthority) -> StartFuture + Send + 'static,
    StartFuture:
        Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>> + Send + 'static,
{
    let supervisor_state = InstallQueueStartSupervisorState::default();
    let transaction_state = state.clone();
    let transaction_selected_queue_id = selected_queue_id.to_string();
    let transaction_producer = producer.claim_child();
    let transaction_owner = transaction_producer.claim_child();
    let transaction_cleanup_foreground = cleanup_foreground.retained();
    let transaction_supervisor_state = supervisor_state.clone();
    let transaction = transaction_owner.spawn_joinable(async move {
        start_selected_queued_install_transaction_with(
            &transaction_state,
            &transaction_selected_queue_id,
            owns_selected_queue,
            &transaction_producer,
            &transaction_cleanup_foreground,
            &transaction_supervisor_state,
            start,
        )
        .await
    });
    let (result_sender, result_receiver) = oneshot::channel();
    let supervisor_owner = producer.claim_child();
    let monitor_producer = producer.claim_child();
    supervisor_owner.spawn(supervise_queue_start_transaction(
        state.clone(),
        supervisor_state,
        transaction,
        monitor_producer,
        result_sender,
    ));
    result_receiver
        .await
        .unwrap_or_else(|_| Err(install_queue_start_stopped_error_response()))
}

async fn start_selected_queued_install_transaction_with<Start, StartFuture>(
    state: &AppState,
    selected_queue_id: &str,
    owns_selected_queue: bool,
    producer: &ProducerLease,
    cleanup_foreground: &IntegrityForegroundLease,
    supervisor_state: &InstallQueueStartSupervisorState,
    mut start: Start,
) -> Result<Option<InstallStartResponse>, InstallApplicationError>
where
    Start: FnMut(InstallQueueSpec, InstallQueueStartAuthority) -> StartFuture,
    StartFuture: Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>>,
{
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let queue_start = state.installs().acquire_queue_start_gate().await;
    let initial_pending = state.installs().queue_snapshot().await.pending.len();
    for _ in 0..initial_pending.saturating_add(1) {
        let entry = match state.installs().reserve_next_queued_install().await {
            crate::state::InstallQueueReservation::Reserved(entry) => {
                supervisor_state.record_reserved(&entry.queue_id);
                entry
            }
            crate::state::InstallQueueReservation::Empty => {
                supervisor_state.clear();
                return selected_queue_residual(state, selected_queue_id, owns_selected_queue)
                    .await;
            }
            crate::state::InstallQueueReservation::BlockedByLiveSession => {
                supervisor_state.clear();
                return Ok(None);
            }
        };
        if settle_unmet_queue_prerequisite(
            state,
            producer,
            cleanup_foreground,
            &entry,
            &update_admission,
        )
        .await
        {
            if entry.queue_id == selected_queue_id {
                supervisor_state.clear();
                return Err(selected_queue_missing_error_response());
            }
            supervisor_state.clear();
            continue;
        }
        let authority = queue_start.authorize(&entry.queue_id);
        match start(entry.spec.clone(), authority.clone()).await {
            Ok(started) => {
                if !state
                    .installs()
                    .queued_install_start_matches(&authority, &started.install_id)
                    .await
                {
                    return Err(install_queue_start_stopped_error_response());
                }
                return Ok(Some(started));
            }
            Err(InstallQueueStartFailure::BlockedByLiveSession) => {
                if !state
                    .installs()
                    .release_active_queued_install_to_front(&entry.queue_id)
                    .await
                {
                    return Err(install_queue_start_stopped_error_response());
                }
                supervisor_state.clear();
                return Ok(None);
            }
            Err(InstallQueueStartFailure::Application(error)) => {
                match state
                    .installs()
                    .settle_queued_install_start_failure(&authority)
                    .await
                {
                    InstallQueueStartFailureDisposition::CompletedUnlinked => {
                        supervisor_state.clear();
                        if entry.queue_id == selected_queue_id {
                            return Err(error);
                        }
                    }
                    InstallQueueStartFailureDisposition::Started { .. } => return Err(error),
                    InstallQueueStartFailureDisposition::Settled => {
                        supervisor_state.clear();
                        return Err(install_queue_start_stopped_error_response());
                    }
                }
            }
        }
    }
    supervisor_state.clear();
    selected_queue_residual(state, selected_queue_id, owns_selected_queue).await
}

async fn settle_unmet_queue_prerequisite(
    state: &AppState,
    producer: &ProducerLease,
    cleanup_foreground: &IntegrityForegroundLease,
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
        let _ = remove_pristine_setup_instance_admitted(
            state,
            producer.claim_child(),
            cleanup_foreground.retained(),
            instance_id,
            cleanup,
            update_admission,
        )
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
    queue_start: &InstallQueueStartAuthority,
) -> Result<InstallStartResponse, InstallQueueStartFailure> {
    match spec {
        InstallQueueSpec::Vanilla { version_id } => {
            start_install_version_with_foreground(
                state,
                InstallVersionStartRequest {
                    version_id: version_id.clone(),
                },
                producer,
                foreground,
                queue_start,
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
                queue_start,
            )
            .await
        }
        InstallQueueSpec::Content {
            instance_id,
            label,
            action,
            ..
        } => {
            start_content_operation(state, instance_id, label, action, producer, queue_start).await
        }
    }
}

async fn start_content_operation(
    state: &AppState,
    instance_id: &str,
    label: &str,
    action: &ContentQueueAction,
    producer: &ProducerLease,
    queue_start: &InstallQueueStartAuthority,
) -> Result<InstallStartResponse, InstallQueueStartFailure> {
    start_content_operation_with_after_journal(
        state,
        instance_id,
        label,
        action,
        producer,
        queue_start,
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
    queue_start: &InstallQueueStartAuthority,
    after_journal: AfterJournal,
) -> Result<InstallStartResponse, InstallQueueStartFailure>
where
    AfterJournal: FnOnce(String, OperationId) -> AfterJournalFuture,
    AfterJournalFuture: Future<Output = ()>,
{
    let cleanup_foreground = state
        .register_integrity_foreground()
        .map_err(|_| install_shutdown_error_response())?;
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(install_update_admission_error_response)?;
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let mut admitted_content = None;
    for _ in 0..OPERATION_ID_RESERVATION_ATTEMPTS {
        let install_id = generate_install_id("content");
        if operation::install_operation_journal_for_session(state.journals(), &install_id).is_some()
        {
            continue;
        }
        let Some(candidate) = mint_available_install_operation_id(state).await else {
            break;
        };
        let admission = InstallAdmissionMarker::new();
        let expected_plan =
            operation::planned_content_journal_for_session(&candidate, &install_id, instance_id);
        let reservation = ContentInitializationReservation::new(
            store.clone(),
            journals.clone(),
            install_id.clone(),
            candidate.clone(),
            expected_plan,
            admission.clone(),
            producer.claim_child(),
        );
        match store
            .admit_queued_content(
                queue_start,
                &admission,
                install_id.clone(),
                candidate.clone(),
            )
            .await
        {
            InstallQueueAdmission::Inserted => {
                admitted_content = Some((install_id, candidate, reservation));
                break;
            }
            InstallQueueAdmission::BlockedByLiveSession => {
                drop(reservation);
                return Err(InstallQueueStartFailure::BlockedByLiveSession);
            }
            InstallQueueAdmission::InstallIdCollision
            | InstallQueueAdmission::OperationIdCollision => {
                drop(reservation);
            }
            InstallQueueAdmission::Existing { .. } => {
                drop(reservation);
                return Err(install_queue_start_stopped_error_response().into());
            }
            InstallQueueAdmission::ReservationLost => {
                drop(reservation);
                return Err(install_queue_start_stopped_error_response().into());
            }
        }
    }
    let Some((install_id, operation_id, initialization)) = admitted_content else {
        return Err(install_journal_error_response().into());
    };
    let initialization = begin_content_journal_with_owned_reconciliation(
        initialization,
        instance_id.to_string(),
        producer,
    )
    .await
    .map_err(|_| InstallQueueStartFailure::Application(install_journal_error_response()))?;
    after_journal(install_id.clone(), operation_id.clone()).await;
    let worker_state = state.clone();
    let worker_store = state.installs().clone();
    let worker_journals = state.journals().clone();
    let progress_journals = worker_journals.clone();
    let worker_install_id = install_id.clone();
    let worker_operation_id = operation_id.clone();
    let progress_operation_id = operation_id.clone();
    let worker_instance_id = instance_id.to_string();
    let worker_action = action.clone();
    let cleanup_foreground = cleanup_foreground.wait_for_settlement().await;
    let worker_cleanup_foreground = cleanup_foreground.retained();
    let interrupted_cleanup_foreground = cleanup_foreground.retained();
    let download_facts = Arc::new(Mutex::new(ContentDownloadFactAccumulator::default()));
    let worker_operation_owner = producer.claim_child();
    let progress_owner = producer.claim_child();
    let worker_guardian_owner = producer.claim_child();
    let worker_cleanup_owner = producer.claim_child();
    let interrupted_guardian_owner = producer.claim_child();
    let interrupted_cleanup_owner = producer.claim_child();
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
    let worker_initialization = initialization;
    InstallStore::spawn_tracked_worker_with_interrupt_progress_owned(
        state.installs().clone(),
        producer.claim_child(),
        install_id.clone(),
        content_interrupted_progress(false),
        async move {
            worker_initialization.hand_off();
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let progress_store = worker_store.clone();
            let progress_install_id = worker_install_id.clone();
            let progress_task = {
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let mut progress_journal = InstallProgressJournalTracker::default();
                    while let Some(progress) = progress_rx.recv().await {
                        if progress.done {
                            continue;
                        }
                        if record_content_operation_progress(
                            &progress_journals,
                            &progress_operation_id,
                            &progress,
                            None,
                            &mut progress_journal,
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

            let operation = if content_action_owns_instance(&worker_action)
                && !instance_version_is_installed_and_launchable(
                    &worker_state,
                    &worker_operation_owner,
                    &worker_instance_id,
                )
                .await
            {
                None
            } else {
                Some(match &worker_action {
                    ContentQueueAction::Install {
                        selections,
                        allow_incompatible,
                        ..
                    } => {
                        let progress_tx = progress_tx.clone();
                        let download_facts = download_facts.clone();
                        crate::application::content::start_content_install_task(
                            worker_operation_owner,
                            worker_state.clone(),
                            crate::application::content::ContentInstallRequest {
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
                            },
                            move |progress| {
                                let _ = progress_tx.send(progress);
                            },
                            move |fact| {
                                download_facts
                                    .lock()
                                    .expect("content download fact accumulator lock poisoned")
                                    .record(fact);
                            },
                        )
                    }
                    ContentQueueAction::Uninstall { canonical_ids } => {
                        let progress_tx = progress_tx.clone();
                        crate::application::content::start_content_uninstall_task(
                            worker_operation_owner,
                            worker_state.clone(),
                            worker_instance_id.clone(),
                            canonical_ids.clone(),
                            move |progress| {
                                let _ = progress_tx.send(progress);
                            },
                        )
                    }
                    ContentQueueAction::Modpack {
                        canonical_id,
                        version_id,
                        selected_file_ids,
                        include_overrides,
                        ..
                    } => {
                        let progress_tx = progress_tx.clone();
                        let download_facts = download_facts.clone();
                        crate::application::content::start_modpack_install_task(
                            worker_operation_owner,
                            worker_state.clone(),
                            crate::application::content::ModpackInstallRequest {
                                instance_id: worker_instance_id.clone(),
                                canonical_id: canonical_id.clone(),
                                version_id: Some(version_id.clone()),
                                selected_file_ids: selected_file_ids.clone(),
                                include_overrides: *include_overrides,
                            },
                            move |progress| {
                                let _ = progress_tx.send(progress);
                            },
                            move |fact| {
                                download_facts
                                    .lock()
                                    .expect("content download fact accumulator lock poisoned")
                                    .record(fact);
                            },
                        )
                    }
                })
            };
            let result = match operation {
                Some(operation) => {
                    let cancellation = operation.cancellation_sender();
                    let joined = operation.join();
                    tokio::pin!(joined);
                    tokio::select! {
                        biased;
                        () = journal_failed.notified() => {
                            if let Some(cancellation) = cancellation {
                                cancellation.cancel();
                            }
                            let _ = joined.await;
                            None
                        }
                        result = &mut joined => Some(result),
                    }
                }
                None => Some(Err(
                    crate::application::content::ContentExecutionError::from((
                        StatusCode::PRECONDITION_FAILED,
                        Json(serde_json::json!({
                            "error": "Minecraft or the selected mod loader did not finish installing."
                        })),
                    )),
                )),
            };
            drop(progress_tx);
            let progress_complete = finish_install_progress_task(progress_task).await;
            let Some(result) = result else {
                return;
            };
            if !progress_complete {
                return;
            }

            let (terminal, failure_kind) = match result {
                Ok(()) => (content_progress("done", 1, 1, true, None), None),
                Err(error) => {
                    let ((_, Json(body)), failure_kind) = error.into_parts();
                    let removed_instance = match content_action_setup_cleanup(&worker_action) {
                        Some(cleanup) => {
                            remove_pristine_setup_instance(
                                &worker_state,
                                worker_cleanup_owner,
                                worker_cleanup_foreground,
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
            let (facts, metrics) = {
                let download_facts = download_facts
                    .lock()
                    .expect("content download fact accumulator lock poisoned");
                (download_facts.facts(), download_facts.metrics())
            };
            let metrics = match metrics {
                Ok(metrics) => metrics,
                Err(_) => {
                    tracing::warn!("content download metrics could not be projected");
                    return;
                }
            };
            *attempted_terminal
                .lock()
                .expect("content terminal progress lock poisoned") =
                Some((terminal.clone(), failure_kind));
            if commit_and_emit_content_terminal_progress(
                worker_store.as_ref(),
                &worker_guardian_owner,
                worker_journals.clone(),
                worker_state.failure_memory().clone(),
                ContentTerminalProgress {
                    operation_id: &worker_operation_id,
                    install_id: &worker_install_id,
                    progress: terminal,
                    execution_facts: &facts,
                    metrics: &metrics,
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
                interrupted_cleanup_owner,
                interrupted_cleanup_foreground,
                &interrupted_instance_id,
                interrupted_setup_cleanup.as_ref(),
            )
            .await;
            let (facts, metrics) = {
                let download_facts = interrupted_download_facts
                    .lock()
                    .expect("content download fact accumulator lock poisoned");
                (download_facts.facts(), download_facts.metrics())
            };
            let Ok(metrics) = metrics else {
                tracing::warn!("content download metrics could not be projected");
                return None;
            };
            let attempted_terminal = interrupted_attempted_terminal
                .lock()
                .expect("content terminal progress lock poisoned")
                .clone();
            settle_content_worker_interruption(
                &interrupted_guardian_owner,
                interrupted_journals.clone(),
                interrupted_state.failure_memory().clone(),
                ContentWorkerInterruptionRequest {
                    operation_id: &interrupted_operation_id,
                    fallback: progress,
                    metrics: &metrics,
                    execution_facts: &facts,
                    attempted_terminal,
                },
            )
            .await
        },
    );

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
    metrics: &'a crate::state::contracts::ContentDownloadMetrics,
    failure_kind: Option<crate::application::content::ContentExecutionFailureKind>,
}

async fn commit_and_emit_content_terminal_progress(
    store: &InstallStore,
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
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
            producer,
            journals.clone(),
            failure_memory,
            ContentFailureOutcomeRequest {
                operation_id: terminal.operation_id,
                download_facts: terminal.execution_facts,
                additional_evidence: evidence,
                phase,
                terminal_progress: &terminal.progress,
                metrics: terminal.metrics,
            },
        )
        .await?;
    } else {
        let mut progress_journal = InstallProgressJournalTracker::default();
        record_content_operation_progress(
            &journals,
            terminal.operation_id,
            &terminal.progress,
            Some(terminal.metrics),
            &mut progress_journal,
        )
        .await?;
    }
    store
        .emit(
            terminal.install_id,
            sanitize_install_progress(terminal.progress),
        )
        .await;
    Ok(())
}

struct ContentWorkerInterruptionRequest<'a> {
    operation_id: &'a OperationId,
    fallback: DownloadProgress,
    metrics: &'a crate::state::contracts::ContentDownloadMetrics,
    execution_facts: &'a [axial_minecraft::download::ExecutionDownloadFact],
    attempted_terminal: Option<(
        DownloadProgress,
        Option<crate::application::content::ContentExecutionFailureKind>,
    )>,
}

async fn settle_content_worker_interruption(
    producer: &ProducerLease,
    journals: Arc<OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    request: ContentWorkerInterruptionRequest<'_>,
) -> Option<DownloadProgress> {
    let ContentWorkerInterruptionRequest {
        operation_id,
        fallback,
        metrics,
        execution_facts,
        attempted_terminal,
    } = request;
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
                producer,
                journals.clone(),
                failure_memory.clone(),
                ContentFailureOutcomeRequest {
                    operation_id,
                    download_facts: execution_facts,
                    additional_evidence: evidence,
                    phase,
                    terminal_progress: attempted_progress,
                    metrics,
                },
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
            return Some(sanitize_install_progress(attempted_progress.clone()));
        }
        match record_content_operation_interrupted(
            &journals,
            operation_id,
            &fallback,
            metrics,
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
                            metrics,
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
) -> (ExecutionFact, OperationPhase) {
    use crate::application::content::ContentExecutionFailureKind;
    let (target, failure_kind, phase) = match kind {
        ContentExecutionFailureKind::FileOperation => (
            "content_filesystem",
            ExecutionFactKind::DownloadTempWriteFailed,
            OperationPhase::Installing,
        ),
        ContentExecutionFailureKind::MetadataInvalid => (
            "content_metadata",
            ExecutionFactKind::ProviderDataInvalid,
            OperationPhase::Downloading,
        ),
        ContentExecutionFailureKind::NetworkFailure => (
            "content_download",
            ExecutionFactKind::DownloadNetworkFailure,
            OperationPhase::Downloading,
        ),
        ContentExecutionFailureKind::PermissionDenied => (
            "content_filesystem",
            ExecutionFactKind::FilePermissionDenied,
            OperationPhase::Installing,
        ),
        ContentExecutionFailureKind::ProviderFailure => (
            "content_provider",
            ExecutionFactKind::DownloadProviderFailure,
            OperationPhase::Downloading,
        ),
    };
    (
        operation::install_execution_fact(
            operation_id,
            target,
            crate::state::contracts::OwnershipClass::LauncherManaged,
            failure_kind,
            [],
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
    cleanup_owner: ProducerLease,
    cleanup_foreground: IntegrityForegroundLease,
    instance_id: &str,
    setup_cleanup: Option<&SetupInstanceCleanup>,
) -> DownloadProgress {
    let removed = match setup_cleanup {
        Some(cleanup) => {
            remove_pristine_setup_instance(
                state,
                cleanup_owner,
                cleanup_foreground,
                instance_id,
                cleanup,
            )
            .await
        }
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
    spawn_install_queue_monitor_owned_with(
        state,
        install_id,
        producer,
        |state, spec, producer, queue_start| async move {
            start_queued_install(&state, &spec, &producer, None, &queue_start).await
        },
    );
}

fn spawn_recovered_install_queue_wake_owned(
    state: AppState,
    install_id: String,
    producer: ProducerLease,
) {
    spawn_recovered_install_queue_wake_owned_with(
        state,
        install_id,
        producer,
        |state, spec, producer, queue_start| async move {
            start_queued_install(&state, &spec, &producer, None, &queue_start).await
        },
    );
}

fn spawn_recovered_install_queue_wake_owned_with<Start, StartFuture>(
    state: AppState,
    install_id: String,
    producer: ProducerLease,
    start: Start,
) where
    Start: FnOnce(AppState, InstallQueueSpec, ProducerLease, InstallQueueStartAuthority) -> StartFuture
        + Send
        + 'static,
    StartFuture:
        Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>> + Send + 'static,
{
    let successor_owner = producer.claim_child();
    producer.spawn(async move {
        let mut shutdown = state.subscribe_shutdown();
        if *shutdown.borrow_and_update() {
            return;
        }
        tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                return;
            }
            _ = wait_for_install_terminal(&state, &install_id) => {}
        }
        state.invalidate_installed_versions();
        if *shutdown.borrow_and_update() {
            return;
        }
        let Ok(successor) = successor_owner.try_claim_successor() else {
            return;
        };
        if maybe_start_next_queued_install_owned_with(&state, &successor, start)
            .await
            .is_err()
        {
            tracing::warn!(
                failure_kind = "recovered_install_queue_wake_failed",
                "recovered install could not settle a pending queue successor"
            );
        }
    });
}

fn spawn_install_queue_monitor_owned_with<Start, StartFuture>(
    state: AppState,
    install_id: String,
    producer: ProducerLease,
    start: Start,
) where
    Start: FnOnce(AppState, InstallQueueSpec, ProducerLease, InstallQueueStartAuthority) -> StartFuture
        + Send
        + 'static,
    StartFuture:
        Future<Output = Result<InstallStartResponse, InstallQueueStartFailure>> + Send + 'static,
{
    let successor_owner = producer.claim_child();
    producer.spawn(async move {
        let mut shutdown = state.subscribe_shutdown();
        if *shutdown.borrow_and_update() {
            return;
        }
        let succeeded = tokio::select! {
            biased;
            changed = shutdown.changed() => {
                let _ = changed;
                return;
            }
            succeeded = wait_for_install_terminal(&state, &install_id) => succeeded,
        };
        state.invalidate_installed_versions();
        let Some(_) = state
            .installs()
            .complete_active_queued_install(&install_id, succeeded)
            .await
        else {
            return;
        };
        if *shutdown.borrow_and_update() {
            return;
        }
        let Ok(successor) = successor_owner.try_claim_successor() else {
            return;
        };
        if maybe_start_next_queued_install_owned_with(&state, &successor, start)
            .await
            .is_err()
        {
            tracing::warn!(
                failure_kind = "queue_successor_start_failed",
                "owned queue successor start did not settle successfully"
            );
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

fn install_queue_remove_stopped_error_response() -> InstallApplicationError {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({
            "error": "The queued install removal stopped before cleanup settled. Try again."
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
        operation_id: match install_id.as_deref() {
            Some(install_id) => state.installs().operation_id(install_id).await,
            None => None,
        },
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
    let record = snapshot.and_then(|snapshot| snapshot.latest);
    let Some(record) = record else {
        return InstallProgressViewModel::starting();
    };
    if spec.is_loader() {
        loader_install_progress_record_view_model(&record)
    } else {
        vanilla_install_progress_record_view_model(&record)
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
            label,
            action,
            ..
        } => InstallQueueInstallItemViewModel {
            version_id: instance_id.clone(),
            loader: None,
            content: Some(InstallQueueContentItemViewModel {
                instance_id: instance_id.clone(),
                label: label.clone(),
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

async fn mint_available_install_operation_id(state: &AppState) -> Option<OperationId> {
    for _ in 0..OPERATION_ID_RESERVATION_ATTEMPTS {
        let operation_id = OperationId::mint();
        if state.journals().get(&operation_id).is_none()
            && !state.installs().contains_operation_id(&operation_id).await
        {
            return Some(operation_id);
        }
    }
    None
}

fn generate_install_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{:032x}", nanos)
}

#[cfg(test)]
mod managed_install_settlement_tests {
    use super::*;
    use crate::state::{
        AppStateInit, IdleSweepReservation, IdleSweepTerminal, InstallStore,
        KnownGoodVerificationUnavailable, SessionStore,
    };
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::oneshot;

    struct Fixture {
        root: PathBuf,
        state: AppState,
        instance_id: String,
    }

    impl Fixture {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "axial-managed-install-settlement-{name}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ));
            let library_dir = root.join("library");
            std::fs::create_dir_all(&library_dir).expect("library directory");
            let paths = AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::load_from(paths.clone(), Arc::clone(&root_session)).expect("config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
                .expect("instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    axial_performance::PerformanceManager::load_for_startup(
                        paths.performance_dir(),
                    )
                    .expect("performance"),
                ),
                startup_warnings: Vec::new(),
            });
            state.set_library_dir_for_test(library_dir.to_string_lossy().into_owned());
            let instance = state
                .instances()
                .insert_for_test("Managed install settlement", "1.21.5")
                .expect("instance");
            let inventory = KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
                root: TestKnownGoodRoot::Versions,
                path: "client.jar".to_string(),
                kind: KnownGoodArtifactKind::ClientJar,
                integrity: TestKnownGoodIntegrity::File { size: 1 },
            }])
            .expect("known-good inventory");
            state.activate_known_good_inventory_for_test(&instance.id, inventory);
            Self {
                root,
                state,
                instance_id: instance.id,
            }
        }

        fn reserve_sweep(&self) -> IdleSweepReservation {
            let epoch = self.state.subscribe_integrity_idle().borrow().epoch();
            let producer = self
                .state
                .try_claim_producer()
                .expect("idle sweep producer");
            self.state
                .try_reserve_idle_sweep(epoch, producer)
                .expect("idle sweep reservation")
        }

        async fn close(self) {
            self.state
                .shutdown()
                .await
                .expect("shut down fixture state");
            drop(self.state);
            let _ = std::fs::remove_dir_all(self.root);
        }
    }

    pub(super) async fn assert_journal_failure_retains_mutation(name: &str) {
        let fixture = Fixture::new(name);
        let reservation = fixture.reserve_sweep();
        let mutation = fixture
            .state
            .admit_managed_artifact_mutation()
            .expect("managed mutation admission");
        let (install_started_tx, install_started_rx) = oneshot::channel();
        let (release_install_tx, release_install_rx) = oneshot::channel();
        let (fail_journal_tx, fail_journal_rx) = oneshot::channel();
        let (journal_selected_tx, journal_selected_rx) = oneshot::channel();
        let install = async move {
            install_started_tx.send(()).expect("signal install start");
            release_install_rx.await.expect("release install");
        };
        let journal_failure = async move {
            fail_journal_rx.await.expect("fail journal");
            journal_selected_tx
                .send(())
                .expect("signal journal selection");
        };
        let settlement = tokio::spawn(await_managed_install_settlement_retaining(
            mutation,
            install,
            journal_failure,
        ));

        install_started_rx.await.expect("install started");
        fail_journal_tx.send(()).expect("trigger journal failure");
        journal_selected_rx.await.expect("journal failure selected");
        assert!(!settlement.is_finished());
        assert!(matches!(
            fixture
                .state
                .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance_id)
                .await,
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));

        release_install_tx.send(()).expect("settle install");
        let ((), mutation) = settlement.await.expect("settlement task");
        drop(mutation);
        let ticket = fixture
            .state
            .mint_known_good_tier2_ticket(&reservation.authority(), &fixture.instance_id)
            .await
            .expect("ticket after install settlement");
        drop(ticket);
        reservation.settle(IdleSweepTerminal::Cancelled);
        fixture.close().await;
    }

    #[tokio::test]
    async fn vanilla_journal_failure_retains_mutation_until_install_settles() {
        assert_journal_failure_retains_mutation("vanilla").await;
    }

    #[tokio::test]
    async fn publication_recovery_retries_until_a_definite_terminal() {
        let recovery =
            axial_minecraft::download::ManagedInstallPublicationRecovery::fixture_with_indeterminate_retries_for_test(2);
        let mut request_drain: InstallRequestDrain = Box::pin(std::future::pending());

        assert!(matches!(
            super::converge_managed_install_publication(recovery, &mut request_drain, false).await,
            ManagedPublicationConvergence::Settled(Err(axial_minecraft::DownloadError::Integrity(
                _
            )))
        ));
    }

    #[tokio::test]
    async fn publication_recovery_defers_before_retry_after_request_drain() {
        let lifecycle = crate::state::AppLifecycle::new();
        let producer = lifecycle
            .try_claim_producer()
            .expect("claim recovery producer");
        let mut request_drain: InstallRequestDrain =
            Box::pin(producer.wait_for_request_drain_start());
        lifecycle.begin_quiesce();
        let recovery =
            axial_minecraft::download::ManagedInstallPublicationRecovery::fixture_for_test();

        assert!(matches!(
            super::converge_managed_install_publication(recovery, &mut request_drain, true).await,
            ManagedPublicationConvergence::DeferredNonterminal
        ));

        drop(producer);
        lifecycle
            .wait_for_quiesced()
            .await
            .expect("quiesce after deferred recovery");
    }
}

#[cfg(test)]
mod tests;
