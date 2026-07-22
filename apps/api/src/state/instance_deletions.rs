use super::instance_registry::{
    CommittedInstanceDeletion, InstanceDeletionFilesystemSettlement,
    InstanceDeletionMarkerClearRetry, InstanceDeletionPersistenceFailure,
    InstanceDeletionPersistenceRetry, InstanceDeletionPreparationFailure,
    InstanceDeletionPreparationRetry, InstanceDeletionSettlementFailure,
    InstanceDeletionSettlementRetry, InstanceDeletionStartupRecovery,
    PreparedInstanceDeletion,
};
use super::known_good::KnownGoodRetirementReservation;
use super::performance_managed::ManagedCompositionRetirement;
use super::{
    AppState, InstanceLifecycleLease, ManagedArtifactMutationAdmission, ProducerLease,
    SetupInstanceCleanup,
};
use axial_config::InstanceStoreError;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

const INSTANCE_DELETION_RETRY_INITIAL_DELAY: Duration = Duration::from_millis(250);
const INSTANCE_DELETION_RETRY_MAX_DELAY: Duration = Duration::from_secs(30);
const INSTANCE_DELETION_ORPHAN_SHUTDOWN_ATTEMPTS: usize = 8;
const INSTANCE_DELETION_LOCK_INVARIANT: &str =
    "instance deletion coordinator lock poisoned; exact settlement ownership is unknown";

#[derive(Clone)]
pub(super) struct InstanceDeletionCoordinator {
    gate: Arc<AsyncMutex<()>>,
    phase: Arc<AtomicU8>,
    retained: Arc<Mutex<Option<RetainedInstanceDeletion>>>,
}

#[must_use = "instance deletion admission must be retained through transaction settlement"]
pub(super) struct InstanceDeletionAdmission {
    _gate: OwnedMutexGuard<()>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InstanceDeletionStartupOutcome {
    Settled,
    Active,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum InstanceDeletionStartupOwnership {
    Pending,
    AppOwned,
    WaiterLost,
}

pub(super) struct InstanceDeletionStartupWaiter {
    ownership: tokio::sync::watch::Sender<InstanceDeletionStartupOwnership>,
    app_owned: bool,
}

impl InstanceDeletionStartupWaiter {
    pub(super) fn pending() -> (
        Self,
        tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>,
    ) {
        let (ownership, receiver) =
            tokio::sync::watch::channel(InstanceDeletionStartupOwnership::Pending);
        (
            Self {
                ownership,
                app_owned: false,
            },
            receiver,
        )
    }

    pub(super) fn mark_app_owned(&mut self) {
        let _ = self
            .ownership
            .send_replace(InstanceDeletionStartupOwnership::AppOwned);
        self.app_owned = true;
    }
}

impl Drop for InstanceDeletionStartupWaiter {
    fn drop(&mut self) {
        if !self.app_owned {
            let _ = self
                .ownership
                .send_replace(InstanceDeletionStartupOwnership::WaiterLost);
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum InstanceDeletionPhase {
    Running = 0,
    Closing = 1,
    Closed = 2,
}

struct RetainedInstanceDeletion {
    auxiliaries: Option<InstanceDeletionAuxiliaries>,
    phase: RetainedInstanceDeletionPhase,
}

impl RetainedInstanceDeletion {
    fn registry_commit_is_durable(&self) -> bool {
        match &self.phase {
            RetainedInstanceDeletionPhase::Committed(_)
            | RetainedInstanceDeletionPhase::MarkerRetry(_) => true,
            RetainedInstanceDeletionPhase::SettlementRetry { expected, .. } => {
                matches!(*expected, FilesystemSettlementExpectation::Settled)
            }
            RetainedInstanceDeletionPhase::Prepared(_)
            | RetainedInstanceDeletionPhase::PreparationRetry(_)
            | RetainedInstanceDeletionPhase::PersistenceRetry(_)
            | RetainedInstanceDeletionPhase::PreCommitRestoreRetry { .. } => false,
        }
    }
}

enum RetainedInstanceDeletionPhase {
    Prepared(PreparedInstanceDeletion),
    PreparationRetry(InstanceDeletionPreparationRetry),
    PersistenceRetry(InstanceDeletionPersistenceRetry),
    PreCommitRestoreRetry {
        retry: InstanceDeletionSettlementRetry,
        original_error: InstanceStoreError,
    },
    Committed(CommittedInstanceDeletion),
    SettlementRetry {
        retry: InstanceDeletionSettlementRetry,
        expected: FilesystemSettlementExpectation,
    },
    MarkerRetry(InstanceDeletionMarkerClearRetry),
}

#[derive(Clone, Copy)]
enum FilesystemSettlementExpectation {
    Aborted,
    Settled,
}

struct InstanceDeletionAuxiliaries {
    instance_id: String,
    lifecycle: InstanceLifecycleLease,
    _mutation: ManagedArtifactMutationAdmission,
    performance: Option<ManagedCompositionRetirement>,
    known_good: KnownGoodRetirementProgress,
    lifecycle_retired: bool,
    witness_settled: bool,
}

enum KnownGoodRetirementProgress {
    Reserved(KnownGoodRetirementReservation),
    Retry,
    Settled,
}

enum InstanceDeletionAttempt {
    Settled,
    Aborted,
    Failed(InstanceStoreError),
    Retry {
        error: InstanceStoreError,
        deletion: RetainedInstanceDeletion,
    },
}

impl InstanceDeletionCoordinator {
    pub(super) fn new() -> Self {
        Self {
            gate: Arc::new(AsyncMutex::new(())),
            phase: Arc::new(AtomicU8::new(InstanceDeletionPhase::Running as u8)),
            retained: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) async fn admit(
        &self,
        state: &AppState,
    ) -> Result<InstanceDeletionAdmission, InstanceStoreError> {
        let gate = Arc::clone(&self.gate).lock_owned().await;
        if self.phase() != InstanceDeletionPhase::Running {
            return Err(instance_deletion_closed_error());
        }
        if let Some(deletion) = self.take_retained() {
            match drive_deletion_once(state, deletion).await {
                InstanceDeletionAttempt::Settled | InstanceDeletionAttempt::Aborted => {}
                InstanceDeletionAttempt::Failed(error) => {
                    tracing::warn!(
                        error_class = instance_deletion_error_class(&error),
                        "retained instance deletion ended before a new admission"
                    );
                }
                InstanceDeletionAttempt::Retry { error, deletion } => {
                    self.retain(deletion);
                    return Err(error);
                }
            }
        }
        if self.phase() != InstanceDeletionPhase::Running {
            return Err(instance_deletion_closed_error());
        }
        Ok(InstanceDeletionAdmission { _gate: gate })
    }

    pub(super) async fn delete_admitted(
        &self,
        state: &AppState,
        _admission: InstanceDeletionAdmission,
        retry_owner: ProducerLease,
        lifecycle: InstanceLifecycleLease,
        instance_id: String,
        delete_files: bool,
    ) -> Result<(), InstanceStoreError> {
        let auxiliaries = self
            .prepare_auxiliaries(
                state,
                lifecycle,
                instance_id.clone(),
                delete_files,
                false,
            )
            .await?;
        let gate = state.instances.acquire_mutation().await?;
        let deletion = prepare_deletion(
            state,
            auxiliaries,
            instance_id,
            delete_files,
            gate,
        )
        .await?;
        self.drive_request(state, deletion, retry_owner).await
    }

    pub(super) async fn delete_pristine_admitted(
        &self,
        state: &AppState,
        _admission: InstanceDeletionAdmission,
        retry_owner: ProducerLease,
        lifecycle: InstanceLifecycleLease,
        instance_id: String,
        cleanup: &SetupInstanceCleanup,
    ) -> Result<bool, InstanceStoreError> {
        let auxiliaries = self
            .prepare_auxiliaries(state, lifecycle, instance_id.clone(), true, false)
            .await?;
        let gate = state.instances.acquire_mutation().await?;
        let baseline = cleanup
            .baseline
            .as_deref()
            .unwrap_or_else(|| std::process::abort());
        if !state.setup_instance_matches_baseline(baseline) {
            return Ok(false);
        }
        let deletion = prepare_deletion(state, auxiliaries, instance_id, true, gate).await?;
        self.drive_request(state, deletion, retry_owner).await?;
        Ok(true)
    }

    pub(super) fn spawn_startup_recovery(
        &self,
        state: AppState,
        owner: ProducerLease,
        startup_ownership: tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>,
    ) -> tokio::task::JoinHandle<Result<InstanceDeletionStartupOutcome, InstanceStoreError>> {
        let retry_owner = owner.claim_child();
        let coordinator = self.clone();
        owner.spawn_joinable(async move {
            let _gate = Arc::clone(&coordinator.gate).lock_owned().await;
            if coordinator.phase() != InstanceDeletionPhase::Running {
                return Err(instance_deletion_closed_error());
            }

            let pending_instance_id = state
                .instances
                .current()
                .pending_deletions
                .first()
                .map(|pending| pending.instance_id.clone());
            let startup_auxiliaries = match pending_instance_id.as_deref() {
                Some(instance_id) => Some(
                    coordinator
                        .prepare_startup_auxiliaries(&state, instance_id)
                        .await?,
                ),
                None => None,
            };
            let mutation = state.instances.acquire_mutation().await?;
            let recovery = state
                .instances
                .prepare_startup_deletion_recovery_with_gate(mutation)
                .await?;
            let Some(recovery) = recovery else {
                drop(startup_auxiliaries);
                return Ok(InstanceDeletionStartupOutcome::Settled);
            };
            let deletion = match recovery {
                InstanceDeletionStartupRecovery::RestoreLive(retry) => {
                    if startup_auxiliaries.is_some() {
                        std::process::abort();
                    }
                    RetainedInstanceDeletion {
                        auxiliaries: None,
                        phase: RetainedInstanceDeletionPhase::SettlementRetry {
                            retry,
                            expected: FilesystemSettlementExpectation::Aborted,
                        },
                    }
                }
                InstanceDeletionStartupRecovery::CompletePending(committed) => {
                    let auxiliaries = startup_auxiliaries.unwrap_or_else(|| std::process::abort());
                    if auxiliaries.instance_id != committed.instance_id() {
                        std::process::abort();
                    }
                    RetainedInstanceDeletion {
                        auxiliaries: Some(auxiliaries),
                        phase: RetainedInstanceDeletionPhase::Committed(committed),
                    }
                }
            };

            match drive_deletion_once(&state, deletion).await {
                InstanceDeletionAttempt::Settled | InstanceDeletionAttempt::Aborted => {
                    Ok(InstanceDeletionStartupOutcome::Settled)
                }
                InstanceDeletionAttempt::Failed(error) => Err(error),
                InstanceDeletionAttempt::Retry { error: _, deletion } => {
                    coordinator.retain(deletion);
                    coordinator.spawn_retained_driver(
                        state.clone(),
                        retry_owner,
                        Some(startup_ownership),
                    );
                    Ok(InstanceDeletionStartupOutcome::Active)
                }
            }
        })
    }

    pub(super) async fn close(&self, state: AppState) -> Result<(), InstanceStoreError> {
        match self.phase.compare_exchange(
            InstanceDeletionPhase::Running as u8,
            InstanceDeletionPhase::Closing as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(value) if value == InstanceDeletionPhase::Closing as u8 => {}
            Err(value) if value == InstanceDeletionPhase::Closed as u8 => return Ok(()),
            Err(_) => panic!("instance deletion coordinator phase is invalid"),
        }

        let coordinator = self.clone();
        tokio::spawn(async move { coordinator.close_owned(&state).await })
            .await
            .map_err(|_| instance_deletion_owner_stopped_error())?
    }

    async fn close_owned(&self, state: &AppState) -> Result<(), InstanceStoreError> {
        let _gate = Arc::clone(&self.gate).lock_owned().await;
        let Some(deletion) = self.take_retained() else {
            self.phase
                .store(InstanceDeletionPhase::Closed as u8, Ordering::Release);
            return Ok(());
        };
        match drive_deletion_once(state, deletion).await {
            InstanceDeletionAttempt::Settled
            | InstanceDeletionAttempt::Aborted
            | InstanceDeletionAttempt::Failed(_) => {
                if self.has_retained() {
                    std::process::abort();
                }
                self.phase
                    .store(InstanceDeletionPhase::Closed as u8, Ordering::Release);
                Ok(())
            }
            InstanceDeletionAttempt::Retry { error, deletion } => {
                self.retain(deletion);
                Err(error)
            }
        }
    }

    async fn drive_request(
        &self,
        state: &AppState,
        mut deletion: RetainedInstanceDeletion,
        retry_owner: ProducerLease,
    ) -> Result<(), InstanceStoreError> {
        let mut retry_delay = INSTANCE_DELETION_RETRY_INITIAL_DELAY;
        loop {
            match drive_deletion_once(state, deletion).await {
                InstanceDeletionAttempt::Settled => return Ok(()),
                InstanceDeletionAttempt::Aborted => {
                    return Err(instance_deletion_aborted_error());
                }
                InstanceDeletionAttempt::Failed(error) => return Err(error),
                InstanceDeletionAttempt::Retry {
                    error,
                    deletion: retry,
                } => {
                    deletion = retry;
                    if deletion.registry_commit_is_durable() {
                        self.retain(deletion);
                        self.spawn_retained_driver(state.clone(), retry_owner, None);
                        return Ok(());
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(retry_delay) => {}
                        _ = retry_owner.wait_for_request_drain_start() => {
                            self.retain(deletion);
                            return Err(error);
                        }
                    }
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(INSTANCE_DELETION_RETRY_MAX_DELAY);
                }
            }
        }
    }

    fn spawn_retained_driver(
        &self,
        state: AppState,
        owner: ProducerLease,
        startup_ownership: Option<
            tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>,
        >,
    ) {
        let coordinator = self.clone();
        owner.spawn(async move {
            let _gate = Arc::clone(&coordinator.gate).lock_owned().await;
            let Some(deletion) = coordinator.take_retained() else {
                return;
            };
            if let Err(error) = coordinator
                .drive_background(&state, deletion, startup_ownership)
                .await
            {
                tracing::warn!(
                    error_class = instance_deletion_error_class(&error),
                    "instance deletion settlement driver stopped"
                );
            }
        });
    }

    async fn drive_background(
        &self,
        state: &AppState,
        mut deletion: RetainedInstanceDeletion,
        mut startup_ownership: Option<
            tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>,
        >,
    ) -> Result<(), InstanceStoreError> {
        let mut shutdown = state.subscribe_shutdown();
        let mut retry_delay = INSTANCE_DELETION_RETRY_INITIAL_DELAY;
        loop {
            if startup_waiter_was_lost(startup_ownership.as_mut()) {
                self.retain(deletion);
                spawn_instance_deletion_orphan_shutdown(state.clone());
                return Err(instance_deletion_startup_waiter_lost_error());
            }
            match drive_deletion_once(state, deletion).await {
                InstanceDeletionAttempt::Settled | InstanceDeletionAttempt::Aborted => {
                    return Ok(());
                }
                InstanceDeletionAttempt::Failed(error) => return Err(error),
                InstanceDeletionAttempt::Retry {
                    error,
                    deletion: retry,
                } => {
                    deletion = retry;
                    if *shutdown.borrow_and_update() {
                        self.retain(deletion);
                        return Err(error);
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(retry_delay) => {}
                        _ = wait_for_startup_waiter_loss(startup_ownership.as_mut()) => {
                            self.retain(deletion);
                            spawn_instance_deletion_orphan_shutdown(state.clone());
                            return Err(instance_deletion_startup_waiter_lost_error());
                        }
                        changed = shutdown.changed() => {
                            let _ = changed;
                            self.retain(deletion);
                            return Err(error);
                        }
                    }
                    retry_delay = retry_delay
                        .saturating_mul(2)
                        .min(INSTANCE_DELETION_RETRY_MAX_DELAY);
                }
            }
        }
    }

    async fn prepare_auxiliaries(
        &self,
        state: &AppState,
        lifecycle: InstanceLifecycleLease,
        instance_id: String,
        delete_files: bool,
        startup: bool,
    ) -> Result<InstanceDeletionAuxiliaries, InstanceStoreError> {
        let mutation = state.admit_managed_artifact_mutation().map_err(|error| {
            InstanceStoreError::Persistence(io::Error::other(error.to_string()))
        })?;
        state
            .instances
            .retire_managed_game_directory(&instance_id, lifecycle.incarnation())
            .await
            .map_err(InstanceStoreError::Persistence)?;
        let performance = if startup {
            None
        } else if delete_files {
            Some(
                state
                    .performance
                    .retire_managed(&instance_id, lifecycle.retained())
                    .await
                    .map_err(|error| {
                        InstanceStoreError::Persistence(io::Error::other(error.to_string()))
                    })?,
            )
        } else {
            state
                .performance
                .retire_existing_managed(&instance_id, lifecycle.retained())
                .await
                .map_err(|error| {
                    InstanceStoreError::Persistence(io::Error::other(error.to_string()))
                })?
        };
        let known_good = if startup {
            KnownGoodRetirementProgress::Retry
        } else {
            KnownGoodRetirementProgress::Reserved(
                state
                    .known_good
                    .reserve_retirement(&instance_id)
                    .map_err(InstanceStoreError::Persistence)?,
            )
        };
        Ok(InstanceDeletionAuxiliaries {
            instance_id,
            lifecycle,
            _mutation: mutation,
            performance,
            known_good,
            lifecycle_retired: false,
            witness_settled: false,
        })
    }

    async fn prepare_startup_auxiliaries(
        &self,
        state: &AppState,
        instance_id: &str,
    ) -> Result<InstanceDeletionAuxiliaries, InstanceStoreError> {
        let lifecycle = state.acquire_instance_lifecycle(instance_id).await;
        self.prepare_auxiliaries(state, lifecycle, instance_id.to_string(), true, true)
            .await
    }

    fn retain(&self, deletion: RetainedInstanceDeletion) {
        let mut retained = self
            .retained
            .lock()
            .expect(INSTANCE_DELETION_LOCK_INVARIANT);
        if retained.is_some() {
            std::process::abort();
        }
        *retained = Some(deletion);
    }

    fn take_retained(&self) -> Option<RetainedInstanceDeletion> {
        self.retained
            .lock()
            .expect(INSTANCE_DELETION_LOCK_INVARIANT)
            .take()
    }

    fn has_retained(&self) -> bool {
        self.retained
            .lock()
            .expect(INSTANCE_DELETION_LOCK_INVARIANT)
            .is_some()
    }

    fn phase(&self) -> InstanceDeletionPhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == InstanceDeletionPhase::Running as u8 => {
                InstanceDeletionPhase::Running
            }
            value if value == InstanceDeletionPhase::Closing as u8 => {
                InstanceDeletionPhase::Closing
            }
            value if value == InstanceDeletionPhase::Closed as u8 => {
                InstanceDeletionPhase::Closed
            }
            _ => panic!("instance deletion coordinator phase is invalid"),
        }
    }
}

impl InstanceDeletionAuxiliaries {
    async fn commit(&mut self, state: &AppState) -> Result<(), InstanceStoreError> {
        if !self.lifecycle_retired {
            self.lifecycle.retire_incarnation();
            if let Some(performance) = self.performance.take() {
                performance.commit();
            }
            self.lifecycle_retired = true;
        }

        let known_good = std::mem::replace(
            &mut self.known_good,
            KnownGoodRetirementProgress::Settled,
        );
        match known_good {
            KnownGoodRetirementProgress::Reserved(reservation) => {
                if let Err(error) = reservation.commit().await {
                    self.known_good = KnownGoodRetirementProgress::Retry;
                    return Err(InstanceStoreError::Persistence(error));
                }
            }
            KnownGoodRetirementProgress::Retry => {
                if let Err(error) = state
                    .known_good
                    .retry_retirement(&self.instance_id)
                    .await
                {
                    self.known_good = KnownGoodRetirementProgress::Retry;
                    return Err(InstanceStoreError::Persistence(error));
                }
            }
            KnownGoodRetirementProgress::Settled => {}
        }

        if !self.witness_settled {
            state
                .user_mod_witnesses
                .remove(&self.instance_id)
                .await
                .map_err(InstanceStoreError::Persistence)?;
            self.witness_settled = true;
        }
        Ok(())
    }
}

async fn prepare_deletion(
    state: &AppState,
    auxiliaries: InstanceDeletionAuxiliaries,
    instance_id: String,
    delete_files: bool,
    gate: OwnedMutexGuard<()>,
) -> Result<RetainedInstanceDeletion, InstanceStoreError> {
    match state
        .instances
        .prepare_delete_with_gate(instance_id, delete_files, gate)
        .await
    {
        Ok(prepared) => Ok(RetainedInstanceDeletion {
            auxiliaries: Some(auxiliaries),
            phase: RetainedInstanceDeletionPhase::Prepared(prepared),
        }),
        Err(InstanceDeletionPreparationFailure::Refused(error)) => Err(error),
        Err(InstanceDeletionPreparationFailure::Retryable { error: _, retry }) => {
            Ok(RetainedInstanceDeletion {
                auxiliaries: Some(auxiliaries),
                phase: RetainedInstanceDeletionPhase::PreparationRetry(retry),
            })
        }
    }
}

async fn drive_deletion_once(
    state: &AppState,
    mut deletion: RetainedInstanceDeletion,
) -> InstanceDeletionAttempt {
    loop {
        deletion.phase = match deletion.phase {
            RetainedInstanceDeletionPhase::Prepared(prepared) => match prepared.persist().await {
                Ok(committed) => RetainedInstanceDeletionPhase::Committed(committed),
                Err(InstanceDeletionPersistenceFailure::PreAcceptance { error, prepared }) => {
                    match prepared.restore().await {
                        Ok(_) => return InstanceDeletionAttempt::Failed(error),
                        Err(InstanceDeletionSettlementFailure::Retryable {
                            error: restore_error,
                            retry,
                        }) => {
                            deletion.phase =
                                RetainedInstanceDeletionPhase::PreCommitRestoreRetry {
                                    retry,
                                    original_error: error,
                                };
                            return InstanceDeletionAttempt::Retry {
                                error: restore_error,
                                deletion,
                            };
                        }
                        Err(InstanceDeletionSettlementFailure::Marker { .. }) => {
                            std::process::abort()
                        }
                    }
                }
                Err(InstanceDeletionPersistenceFailure::Retryable { error, retry }) => {
                    deletion.phase = RetainedInstanceDeletionPhase::PersistenceRetry(retry);
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
            },
            RetainedInstanceDeletionPhase::PreparationRetry(retry) => match retry.retry().await {
                Ok(prepared) => RetainedInstanceDeletionPhase::Prepared(prepared),
                Err(InstanceDeletionPreparationFailure::Refused(error)) => {
                    return InstanceDeletionAttempt::Failed(error);
                }
                Err(InstanceDeletionPreparationFailure::Retryable { error, retry }) => {
                    deletion.phase = RetainedInstanceDeletionPhase::PreparationRetry(retry);
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
            },
            RetainedInstanceDeletionPhase::PersistenceRetry(retry) => match retry.retry().await {
                Ok(committed) => RetainedInstanceDeletionPhase::Committed(committed),
                Err(InstanceDeletionPersistenceFailure::Retryable { error, retry }) => {
                    deletion.phase = RetainedInstanceDeletionPhase::PersistenceRetry(retry);
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
                Err(InstanceDeletionPersistenceFailure::PreAcceptance { .. }) => {
                    std::process::abort()
                }
            },
            RetainedInstanceDeletionPhase::PreCommitRestoreRetry {
                retry,
                original_error,
            } => match retry.retry().await {
                Ok(InstanceDeletionFilesystemSettlement::Aborted(_)) => {
                    return InstanceDeletionAttempt::Failed(original_error);
                }
                Ok(InstanceDeletionFilesystemSettlement::Settled(_)) => std::process::abort(),
                Err(InstanceDeletionSettlementFailure::Retryable { error, retry }) => {
                    deletion.phase = RetainedInstanceDeletionPhase::PreCommitRestoreRetry {
                        retry,
                        original_error,
                    };
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
                Err(InstanceDeletionSettlementFailure::Marker { .. }) => std::process::abort(),
            },
            RetainedInstanceDeletionPhase::Committed(committed) => {
                let auxiliaries = deletion
                    .auxiliaries
                    .as_mut()
                    .unwrap_or_else(|| std::process::abort());
                if let Err(error) = auxiliaries.commit(state).await {
                    deletion.phase = RetainedInstanceDeletionPhase::Committed(committed);
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
                match committed.settle_files().await {
                    Ok(_) => return InstanceDeletionAttempt::Settled,
                    Err(InstanceDeletionSettlementFailure::Retryable { error, retry }) => {
                        deletion.phase = RetainedInstanceDeletionPhase::SettlementRetry {
                            retry,
                            expected: FilesystemSettlementExpectation::Settled,
                        };
                        return InstanceDeletionAttempt::Retry { error, deletion };
                    }
                    Err(InstanceDeletionSettlementFailure::Marker { error, retry }) => {
                        deletion.phase = RetainedInstanceDeletionPhase::MarkerRetry(retry);
                        return InstanceDeletionAttempt::Retry { error, deletion };
                    }
                }
            }
            RetainedInstanceDeletionPhase::SettlementRetry { retry, expected } => {
                match retry.retry().await {
                    Ok(InstanceDeletionFilesystemSettlement::Aborted(_)) => match expected {
                        FilesystemSettlementExpectation::Aborted => {
                            return InstanceDeletionAttempt::Aborted;
                        }
                        FilesystemSettlementExpectation::Settled => std::process::abort(),
                    },
                    Ok(InstanceDeletionFilesystemSettlement::Settled(_)) => match expected {
                        FilesystemSettlementExpectation::Settled => {
                            return InstanceDeletionAttempt::Settled;
                        }
                        FilesystemSettlementExpectation::Aborted => std::process::abort(),
                    },
                    Err(InstanceDeletionSettlementFailure::Retryable { error, retry }) => {
                        deletion.phase = RetainedInstanceDeletionPhase::SettlementRetry {
                            retry,
                            expected,
                        };
                        return InstanceDeletionAttempt::Retry { error, deletion };
                    }
                    Err(InstanceDeletionSettlementFailure::Marker { error, retry }) => {
                        if matches!(expected, FilesystemSettlementExpectation::Aborted) {
                            std::process::abort();
                        }
                        deletion.phase = RetainedInstanceDeletionPhase::MarkerRetry(retry);
                        return InstanceDeletionAttempt::Retry { error, deletion };
                    }
                }
            }
            RetainedInstanceDeletionPhase::MarkerRetry(retry) => match retry.retry().await {
                Ok(_) => return InstanceDeletionAttempt::Settled,
                Err(InstanceDeletionSettlementFailure::Marker { error, retry }) => {
                    deletion.phase = RetainedInstanceDeletionPhase::MarkerRetry(retry);
                    return InstanceDeletionAttempt::Retry { error, deletion };
                }
                Err(InstanceDeletionSettlementFailure::Retryable { .. }) => std::process::abort(),
            },
        };
    }
}

fn instance_deletion_closed_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::other(
        "instance deletion coordinator is closed",
    ))
}

fn instance_deletion_aborted_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::other(
        "instance deletion ended before registry commit",
    ))
}

pub(super) fn instance_deletion_owner_stopped_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::other(
        "instance deletion owner stopped before reporting completion",
    ))
}

fn instance_deletion_startup_waiter_lost_error() -> InstanceStoreError {
    InstanceStoreError::Persistence(io::Error::other(
        "instance deletion startup waiter was lost",
    ))
}

fn startup_waiter_was_lost(
    ownership: Option<&mut tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>>,
) -> bool {
    ownership.is_some_and(|ownership| {
        *ownership.borrow_and_update() == InstanceDeletionStartupOwnership::WaiterLost
    })
}

async fn wait_for_startup_waiter_loss(
    ownership: Option<&mut tokio::sync::watch::Receiver<InstanceDeletionStartupOwnership>>,
) {
    let Some(ownership) = ownership else {
        std::future::pending::<()>().await;
        return;
    };
    loop {
        match *ownership.borrow_and_update() {
            InstanceDeletionStartupOwnership::WaiterLost => return,
            InstanceDeletionStartupOwnership::AppOwned => {
                std::future::pending::<()>().await;
            }
            InstanceDeletionStartupOwnership::Pending => {}
        }
        if ownership.changed().await.is_err() {
            std::future::pending::<()>().await;
        }
    }
}

fn spawn_instance_deletion_orphan_shutdown(state: AppState) {
    tokio::spawn(async move {
        let mut delay = INSTANCE_DELETION_RETRY_INITIAL_DELAY;
        for _ in 0..INSTANCE_DELETION_ORPHAN_SHUTDOWN_ATTEMPTS {
            if state.shutdown().await.is_ok() {
                return;
            }
            tokio::time::sleep(delay).await;
            delay = delay
                .saturating_mul(2)
                .min(INSTANCE_DELETION_RETRY_MAX_DELAY);
        }
        std::process::abort();
    });
}

fn instance_deletion_error_class(error: &InstanceStoreError) -> &'static str {
    match error {
        InstanceStoreError::Root(_) => "root",
        InstanceStoreError::Read(_) => "read",
        InstanceStoreError::Parse(_) => "parse",
        InstanceStoreError::Validation(_) => "validation",
        InstanceStoreError::TooLarge { .. } => "too_large",
        InstanceStoreError::Persistence(_) => "persistence",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_carriers_are_send_and_static() {
        fn assert_send_static<T: Send + 'static>() {}

        assert_send_static::<RetainedInstanceDeletion>();
        assert_send_static::<InstanceDeletionAuxiliaries>();
    }

    #[test]
    fn dropped_startup_waiter_publishes_terminal_loss() {
        let (waiter, mut ownership) = InstanceDeletionStartupWaiter::pending();

        drop(waiter);

        assert_eq!(
            *ownership.borrow_and_update(),
            InstanceDeletionStartupOwnership::WaiterLost
        );
    }

    #[test]
    fn returned_state_publishes_terminal_app_ownership() {
        let (mut waiter, mut ownership) = InstanceDeletionStartupWaiter::pending();

        waiter.mark_app_owned();
        drop(waiter);

        assert_eq!(
            *ownership.borrow_and_update(),
            InstanceDeletionStartupOwnership::AppOwned
        );
    }

}
