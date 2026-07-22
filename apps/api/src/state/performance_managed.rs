use axial_performance::{
    CompositionPlan, ManagedCompositionAuthority, ManagedCompositionInspection,
    ManagedCompositionInstallPlan, ManagedInstallExecutionError, ManagedInstallExecutionOutcome,
    ManagedInstanceEffectAuthority, ManagedInstanceIdentity, ManagedMutationError, ManagedResolvedInspection,
    ManagedRollbackOutcome, ResolutionRequest,
};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, RwLock as AsyncRwLock,
};

const MANAGED_OWNER_LOCK_INVARIANT: &str =
    "managed composition owner lock poisoned; admission state may be inconsistent";

type ManagedEntries = HashMap<String, ManagedEntrySlot>;

struct ManagedEntrySlot {
    entry: Weak<ManagedInstanceEntry>,
    retained: Option<Arc<ManagedInstanceEntry>>,
}

struct ManagedOperationLatch {
    entries: Arc<Mutex<ManagedEntries>>,
    entry: Arc<ManagedInstanceEntry>,
    armed: bool,
}

impl ManagedOperationLatch {
    fn new(entries: Arc<Mutex<ManagedEntries>>, entry: Arc<ManagedInstanceEntry>) -> Self {
        Self {
            entries,
            entry,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for ManagedOperationLatch {
    fn drop(&mut self) {
        if self.armed {
            publish_entry_phase(&self.entries, &self.entry, ManagedEntryPhase::Latched);
        }
    }
}

struct ManagedInstanceEntry {
    identity: ManagedInstanceIdentity,
    effects: OnceLock<ManagedInstanceEffectAuthority>,
    gate: Arc<AsyncMutex<()>>,
    work_gate: Arc<AsyncMutex<()>>,
    phase: AtomicU8,
}

pub(super) struct ManagedCompositionOwner {
    authority: ManagedCompositionAuthority,
    managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    entries: Arc<Mutex<ManagedEntries>>,
    lifecycle: Arc<AsyncRwLock<()>>,
    instance_lifecycle: super::instance_lifecycle::InstanceLifecycleGates,
    close_gate: Arc<AsyncMutex<()>>,
    phase: AtomicU8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ManagedEntryPhase {
    Open = 0,
    Latched = 1,
    Retired = 2,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum ManagedOwnerPhase {
    Running = 0,
    Closing = 1,
    Closed = 2,
}

pub(crate) struct AppManagedCompositionAdmission {
    authority: ManagedCompositionAuthority,
    managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    entries: Arc<Mutex<ManagedEntries>>,
    entry: Arc<ManagedInstanceEntry>,
    instance_lifecycle: super::InstanceLifecycleLease,
    _lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    _gate: OwnedMutexGuard<()>,
}

pub(crate) struct ManagedCompositionRetirement {
    entries: Arc<Mutex<ManagedEntries>>,
    entry: Arc<ManagedInstanceEntry>,
    _instance_lifecycle: super::InstanceLifecycleLease,
    _lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    _gate: OwnedMutexGuard<()>,
    committed: bool,
}

impl ManagedCompositionRetirement {
    pub(crate) fn commit(mut self) {
        self.committed = true;
        let instance_id = self.entry.identity.instance_id();
        let mut entries = self.entries.lock().expect(MANAGED_OWNER_LOCK_INVARIANT);
        if entries
            .get(instance_id)
            .and_then(|slot| slot.entry.upgrade())
            .is_some_and(|current| Arc::ptr_eq(&current, &self.entry))
        {
            entries.remove(instance_id);
        }
    }
}

impl Drop for ManagedCompositionRetirement {
    fn drop(&mut self) {
        if !self.committed {
            publish_entry_phase(&self.entries, &self.entry, ManagedEntryPhase::Open);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedCompositionAdmissionError {
    #[error("managed composition admission is closed")]
    Closed,
    #[error("managed composition exact recovery could not prove a clean state")]
    RecoveryFailed,
    #[error("managed composition recovery is blocked while the instance is running")]
    RecoveryBlockedByActiveSession,
    #[error("managed composition identity is retired")]
    Retired,
    #[error("managed composition lifecycle lease belongs to another application state")]
    ForeignLifecycleAuthority,
    #[error("managed composition identity is invalid: {0}")]
    Identity(#[from] axial_performance::ManagedIdentityError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedInstanceAdmissionError {
    #[error("instance not found")]
    InstanceNotFound,
    #[error("instance identity is invalid")]
    InvalidInstanceIdentity,
    #[error("managed composition mutation is blocked while the instance is running")]
    ActiveSession,
    #[error(
        "managed composition admission requires foreground authority from this application state"
    )]
    ForeignForegroundAuthority,
    #[error("{0}")]
    Owner(#[from] ManagedCompositionAdmissionError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedInspectionError {
    #[error("{0}")]
    Admission(#[from] ManagedInstanceAdmissionError),
    #[error("{0}")]
    Operation(#[from] ManagedMutationError),
}

#[derive(Debug, thiserror::Error)]
#[error("managed composition shutdown is blocked by reconciliation-required instances")]
pub(crate) struct ManagedCompositionCloseError;

impl ManagedCompositionOwner {
    pub(super) fn claim(
        authority: ManagedCompositionAuthority,
        instance_lifecycle: super::instance_lifecycle::InstanceLifecycleGates,
        managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    ) -> Self {
        Self {
            authority,
            managed_artifact_epoch,
            entries: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(AsyncRwLock::new(())),
            instance_lifecycle,
            close_gate: Arc::new(AsyncMutex::new(())),
            phase: AtomicU8::new(ManagedOwnerPhase::Running as u8),
        }
    }

    pub(super) async fn admit(
        &self,
        instance_id: &str,
        instance_lifecycle: super::InstanceLifecycleLease,
        recovery_allowed: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedCompositionAdmissionError> {
        if !instance_lifecycle.matches(instance_id) {
            return Err(ManagedCompositionAdmissionError::Identity(
                axial_performance::ManagedIdentityError::InvalidInstanceId,
            ));
        }
        if !self.instance_lifecycle.owns(&instance_lifecycle.owner) {
            return Err(ManagedCompositionAdmissionError::ForeignLifecycleAuthority);
        }
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let lifecycle = Arc::new(self.lifecycle.clone().read_owned().await);
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let entry = self.entry(instance_id)?;
        let gate = entry.gate.clone().lock_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        self.bind_entry_effects(
            &entry,
            instance_lifecycle.retained(),
            lifecycle.clone(),
        )
        .await?;
        drop(entry.work_gate.clone().lock_owned().await);
        if entry.phase() == ManagedEntryPhase::Open
            && entry.effects().require_settled().is_err()
        {
            publish_entry_phase(&self.entries, &entry, ManagedEntryPhase::Latched);
        }
        let admission = AppManagedCompositionAdmission {
            authority: self.authority.clone(),
            managed_artifact_epoch: self.managed_artifact_epoch.clone(),
            entries: self.entries.clone(),
            entry: entry.clone(),
            instance_lifecycle,
            _lifecycle: lifecycle,
            _gate: gate,
        };
        match entry.phase() {
            ManagedEntryPhase::Open => Ok(admission),
            ManagedEntryPhase::Retired => Err(ManagedCompositionAdmissionError::Retired),
            ManagedEntryPhase::Latched if !recovery_allowed => {
                Err(ManagedCompositionAdmissionError::RecoveryBlockedByActiveSession)
            }
            ManagedEntryPhase::Latched => {
                recover_admission_owned(admission).await
            }
        }
    }

    pub(super) async fn retire(
        &self,
        instance_id: &str,
        instance_lifecycle: super::InstanceLifecycleLease,
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        self.validate_retirement(instance_id, &instance_lifecycle)?;
        let lifecycle = Arc::new(self.lifecycle.clone().read_owned().await);
        let entry = self.entry(instance_id)?;
        self.retire_entry(entry, instance_lifecycle, lifecycle).await
    }

    pub(super) async fn retire_existing(
        &self,
        instance_id: &str,
        instance_lifecycle: super::InstanceLifecycleLease,
    ) -> Result<Option<ManagedCompositionRetirement>, ManagedCompositionAdmissionError> {
        self.validate_retirement(instance_id, &instance_lifecycle)?;
        let lifecycle = Arc::new(self.lifecycle.clone().read_owned().await);
        let Some(entry) = self.existing_entry(instance_id) else {
            return Ok(None);
        };
        self.retire_existing_entry(entry, instance_lifecycle, lifecycle)
            .await
    }

    fn validate_retirement(
        &self,
        instance_id: &str,
        instance_lifecycle: &super::InstanceLifecycleLease,
    ) -> Result<(), ManagedCompositionAdmissionError> {
        if !instance_lifecycle.matches(instance_id) {
            return Err(ManagedCompositionAdmissionError::Identity(
                axial_performance::ManagedIdentityError::InvalidInstanceId,
            ));
        }
        if !self.instance_lifecycle.owns(&instance_lifecycle.owner) {
            return Err(ManagedCompositionAdmissionError::ForeignLifecycleAuthority);
        }
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        Ok(())
    }

    async fn retire_entry(
        &self,
        entry: Arc<ManagedInstanceEntry>,
        instance_lifecycle: super::InstanceLifecycleLease,
        lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        let gate = entry.gate.clone().lock_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        self.bind_entry_effects(
            &entry,
            instance_lifecycle.retained(),
            lifecycle.clone(),
        )
        .await?;
        let work = entry.work_gate.clone().lock_owned().await;
        let authority = self.authority.clone();
        let managed_artifact_epoch = self.managed_artifact_epoch.clone();
        let entries = self.entries.clone();
        tokio::spawn(async move {
            let _work = work;
            let mut latch = ManagedOperationLatch::new(entries.clone(), entry.clone());
            let worker_entry = entry.clone();
            let worker = tokio::spawn(async move {
                let _mutation = managed_artifact_epoch
                    .admit()
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)?;
                authority
                    .recover_and_inspect(&worker_entry.identity, worker_entry.effects())
                    .await
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)
            });
            if !matches!(worker.await, Ok(Ok(_))) {
                return Err(ManagedCompositionAdmissionError::RecoveryFailed);
            }
            match entry.phase() {
                ManagedEntryPhase::Open | ManagedEntryPhase::Latched => {
                    entry.store_phase(ManagedEntryPhase::Retired);
                }
                ManagedEntryPhase::Retired => {
                    latch.disarm();
                    return Err(ManagedCompositionAdmissionError::Retired);
                }
            }
            latch.disarm();
            Ok(ManagedCompositionRetirement {
                entries,
                entry,
                _instance_lifecycle: instance_lifecycle,
                _lifecycle: lifecycle,
                _gate: gate,
                committed: false,
            })
        })
        .await
        .unwrap_or(Err(ManagedCompositionAdmissionError::RecoveryFailed))
    }

    async fn retire_existing_entry(
        &self,
        entry: Arc<ManagedInstanceEntry>,
        instance_lifecycle: super::InstanceLifecycleLease,
        lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    ) -> Result<Option<ManagedCompositionRetirement>, ManagedCompositionAdmissionError> {
        let gate = entry.gate.clone().lock_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let Some(effects) = entry.effects.get().cloned() else {
            return Ok(None);
        };
        let work = entry.work_gate.clone().lock_owned().await;
        let managed_artifact_epoch = self.managed_artifact_epoch.clone();
        let entries = self.entries.clone();
        tokio::spawn(async move {
            let _work = work;
            let mut latch = ManagedOperationLatch::new(entries.clone(), entry.clone());
            let settled = tokio::task::spawn_blocking(move || {
                let _mutation = managed_artifact_epoch
                    .admit()
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)?;
                effects
                    .settle()
                    .and_then(|_| effects.require_settled())
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)
            })
            .await;
            if !matches!(settled, Ok(Ok(()))) {
                return Err(ManagedCompositionAdmissionError::RecoveryFailed);
            }
            match entry.phase() {
                ManagedEntryPhase::Open | ManagedEntryPhase::Latched => {
                    entry.store_phase(ManagedEntryPhase::Retired);
                }
                ManagedEntryPhase::Retired => {
                    latch.disarm();
                    return Err(ManagedCompositionAdmissionError::Retired);
                }
            }
            latch.disarm();
            Ok(Some(ManagedCompositionRetirement {
                entries,
                entry,
                _instance_lifecycle: instance_lifecycle,
                _lifecycle: lifecycle,
                _gate: gate,
                committed: false,
            }))
        })
        .await
        .unwrap_or(Err(ManagedCompositionAdmissionError::RecoveryFailed))
    }

    pub(super) async fn close(&self) -> Result<(), ManagedCompositionCloseError> {
        match self.phase.compare_exchange(
            ManagedOwnerPhase::Running as u8,
            ManagedOwnerPhase::Closing as u8,
            Ordering::AcqRel,
            Ordering::Acquire,
        ) {
            Ok(_) => {}
            Err(value) if value == ManagedOwnerPhase::Closing as u8 => {}
            Err(value) if value == ManagedOwnerPhase::Closed as u8 => return Ok(()),
            Err(_) => panic!("managed composition owner phase is invalid"),
        }
        let _close = self.close_gate.clone().lock_owned().await;
        if self.phase() == ManagedOwnerPhase::Closed {
            return Ok(());
        }
        let drained = self.lifecycle.write().await;
        let mut entries = self
            .entries
            .lock()
            .expect(MANAGED_OWNER_LOCK_INVARIANT)
            .values()
            .filter_map(|slot| slot.entry.upgrade())
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.identity
                .instance_id()
                .cmp(right.identity.instance_id())
        });
        drop(drained);

        let mut recovery_failed = false;
        for entry in entries {
            let gate = entry.gate.clone().lock_owned().await;
            let lifecycle = Arc::new(self.lifecycle.clone().read_owned().await);
            let instance_lifecycle = super::InstanceLifecycleLease::bind(
                entry.identity.instance_id(),
                self.instance_lifecycle.clone(),
                self.instance_lifecycle
                    .acquire(entry.identity.instance_id())
                    .await,
            );
            if self
                .bind_entry_effects(&entry, instance_lifecycle.retained(), lifecycle.clone())
                .await
                .is_err()
            {
                recovery_failed = true;
                continue;
            }
            let work = entry.work_gate.clone().lock_owned().await;
            if !recover_entry_owned(
                self.authority.clone(),
                self.managed_artifact_epoch.clone(),
                self.entries.clone(),
                entry,
                lifecycle,
                instance_lifecycle,
                gate,
                work,
            )
            .await
            {
                recovery_failed = true;
            }
        }
        if recovery_failed {
            Err(ManagedCompositionCloseError)
        } else {
            self.entries
                .lock()
                .expect(MANAGED_OWNER_LOCK_INVARIANT)
                .clear();
            self.phase
                .store(ManagedOwnerPhase::Closed as u8, Ordering::Release);
            Ok(())
        }
    }

    fn entry(
        &self,
        instance_id: &str,
    ) -> Result<Arc<ManagedInstanceEntry>, ManagedCompositionAdmissionError> {
        let mut entries = self.entries.lock().expect(MANAGED_OWNER_LOCK_INVARIANT);
        entries.retain(|_, slot| slot.entry.strong_count() != 0);
        if let Some(entry) = entries
            .get(instance_id)
            .and_then(|slot| slot.entry.upgrade())
        {
            return Ok(entry);
        }
        entries.remove(instance_id);
        let identity = self.authority.identify(instance_id)?;
        let entry = Arc::new(ManagedInstanceEntry {
            identity,
            effects: OnceLock::new(),
            gate: Arc::new(AsyncMutex::new(())),
            work_gate: Arc::new(AsyncMutex::new(())),
            phase: AtomicU8::new(ManagedEntryPhase::Open as u8),
        });
        entries.insert(
            instance_id.to_string(),
            ManagedEntrySlot {
                entry: Arc::downgrade(&entry),
                retained: None,
            },
        );
        Ok(entry)
    }

    fn existing_entry(&self, instance_id: &str) -> Option<Arc<ManagedInstanceEntry>> {
        let mut entries = self.entries.lock().expect(MANAGED_OWNER_LOCK_INVARIANT);
        let entry = entries
            .get(instance_id)
            .and_then(|slot| slot.entry.upgrade());
        if entry.is_none() {
            entries.remove(instance_id);
        }
        entry
    }

    async fn bind_entry_effects(
        &self,
        entry: &Arc<ManagedInstanceEntry>,
        instance_lifecycle: super::InstanceLifecycleLease,
        lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    ) -> Result<(), ManagedCompositionAdmissionError> {
        if entry.effects.get().is_some() {
            return Ok(());
        }
        let work = entry.work_gate.clone().lock_owned().await;
        if entry.effects.get().is_some() {
            return Ok(());
        }
        let authority = self.authority.clone();
        let entry = entry.clone();
        tokio::spawn(async move {
            let _work = work;
            let _lifecycle = lifecycle;
            let _instance_lifecycle = instance_lifecycle;
            let effects = authority
                .bind_instance_effect_authority(&entry.identity)
                .await?;
            let _ = entry.effects.set(effects);
            Ok::<(), ManagedMutationError>(())
        })
        .await
        .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)?
        .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)
    }

    fn phase(&self) -> ManagedOwnerPhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == ManagedOwnerPhase::Running as u8 => ManagedOwnerPhase::Running,
            value if value == ManagedOwnerPhase::Closing as u8 => ManagedOwnerPhase::Closing,
            value if value == ManagedOwnerPhase::Closed as u8 => ManagedOwnerPhase::Closed,
            _ => panic!("managed composition owner phase is invalid"),
        }
    }
}

impl ManagedInstanceEntry {
    fn effects(&self) -> &ManagedInstanceEffectAuthority {
        self.effects
            .get()
            .expect("managed entry effect authority is initialized under its gate")
    }

    fn phase(&self) -> ManagedEntryPhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == ManagedEntryPhase::Open as u8 => ManagedEntryPhase::Open,
            value if value == ManagedEntryPhase::Latched as u8 => ManagedEntryPhase::Latched,
            value if value == ManagedEntryPhase::Retired as u8 => ManagedEntryPhase::Retired,
            _ => panic!("managed composition entry phase is invalid"),
        }
    }

    fn store_phase(&self, phase: ManagedEntryPhase) {
        self.phase.store(phase as u8, Ordering::Release);
    }
}

async fn recover_admission_owned(
    admission: AppManagedCompositionAdmission,
) -> Result<AppManagedCompositionAdmission, ManagedCompositionAdmissionError> {
    let supervisor = tokio::spawn(async move {
        let mut latch =
            ManagedOperationLatch::new(admission.entries.clone(), admission.entry.clone());
        let authority = admission.authority.clone();
        let managed_artifact_epoch = admission.managed_artifact_epoch.clone();
        let entry = admission.entry.clone();
        let recovered = matches!(
            tokio::spawn(async move {
                let _mutation = managed_artifact_epoch
                    .admit()
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)?;
                authority
                    .recover_and_inspect(&entry.identity, entry.effects())
                    .await
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)
            })
            .await,
            Ok(Ok(_))
        );
        if recovered {
            publish_entry_phase(
                &admission.entries,
                &admission.entry,
                ManagedEntryPhase::Open,
            );
            latch.disarm();
            Ok(admission)
        } else {
            Err(ManagedCompositionAdmissionError::RecoveryFailed)
        }
    });
    supervisor
        .await
        .unwrap_or(Err(ManagedCompositionAdmissionError::RecoveryFailed))
}

async fn recover_entry_owned(
    authority: ManagedCompositionAuthority,
    managed_artifact_epoch: super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
    entries: Arc<Mutex<ManagedEntries>>,
    entry: Arc<ManagedInstanceEntry>,
    lifecycle: Arc<OwnedRwLockReadGuard<()>>,
    instance_lifecycle: super::InstanceLifecycleLease,
    gate: OwnedMutexGuard<()>,
    work: OwnedMutexGuard<()>,
) -> bool {
    let supervisor = tokio::spawn(async move {
        let _lifecycle = lifecycle;
        let _instance_lifecycle = instance_lifecycle;
        let _gate = gate;
        let _work = work;
        let mut latch = ManagedOperationLatch::new(entries.clone(), entry.clone());
        let worker_entry = entry.clone();
        let recovered = matches!(
            tokio::spawn(async move {
                let _mutation = managed_artifact_epoch
                    .admit()
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)?;
                authority
                    .recover_and_inspect(&worker_entry.identity, worker_entry.effects())
                    .await
                    .map_err(|_| ManagedCompositionAdmissionError::RecoveryFailed)
            })
            .await,
            Ok(Ok(_))
        );
        if recovered {
            publish_entry_phase(&entries, &entry, ManagedEntryPhase::Open);
            latch.disarm();
        }
        recovered
    });
    supervisor.await.unwrap_or(false)
}

impl AppManagedCompositionAdmission {
    pub(crate) async fn composition_managed_witness_proofs(
        &self,
    ) -> Result<Vec<axial_performance::ManagedArtifactWitnessProof>, ManagedMutationError> {
        self.run_owned("composition_managed_witness_proofs", |authority, identity, effects, _| async move {
            authority
                .composition_managed_witness_proofs(&identity, &effects)
                .await
        })
        .await
    }

    pub(crate) async fn inspect(
        &self,
        plan: Option<&CompositionPlan>,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        let plan = plan.cloned();
        self.run_owned("inspect", move |authority, identity, effects, managed_artifact_epoch| async move {
            authority
                .inspect(&identity, &effects, plan.as_ref(), move || {
                    managed_mutation_admission(&managed_artifact_epoch)
                })
                .await
        })
        .await
    }

    pub(crate) async fn resolve_and_inspect(
        &self,
        request: ResolutionRequest,
    ) -> Result<ManagedResolvedInspection, ManagedMutationError> {
        self.run_owned("resolve_and_inspect", move |authority, identity, effects, managed_artifact_epoch| async move {
            authority
                .resolve_and_inspect(&identity, &effects, request, move || {
                    managed_mutation_admission(&managed_artifact_epoch)
                })
                .await
        })
        .await
    }

    pub(crate) async fn ensure_installed<
        BeforeTargetEffect,
        BeforeTargetEffectFuture,
        BeforeTargetEffectError,
    >(
        &self,
        plan: &ManagedCompositionInstallPlan,
        client: &reqwest::Client,
        before_target_effect: BeforeTargetEffect,
    ) -> Result<ManagedInstallExecutionOutcome, ManagedInstallExecutionError<BeforeTargetEffectError>>
    where
        BeforeTargetEffect: FnOnce() -> BeforeTargetEffectFuture + Send + 'static,
        BeforeTargetEffectFuture:
            std::future::Future<Output = Result<(), BeforeTargetEffectError>> + Send + 'static,
        BeforeTargetEffectError: Send + 'static,
    {
        let work = self.entry.work_gate.clone().lock_owned().await;
        let effects = self.entry.effects().clone();
        if self.entry.phase() != ManagedEntryPhase::Open || effects.require_settled().is_err() {
            publish_entry_phase(&self.entries, &self.entry, ManagedEntryPhase::Latched);
            return Err(ManagedInstallExecutionError::from_mutation(
                ManagedMutationError::reconciliation_required("install"),
                false,
            ));
        }
        let lifecycle = self._lifecycle.clone();
        let instance_lifecycle = self.instance_lifecycle.retained();
        let authority = self.authority.clone();
        let identity = self.entry.identity.clone();
        let effects_after = effects.clone();
        let managed_artifact_epoch = self.managed_artifact_epoch.clone();
        let plan = plan.clone();
        let client = client.clone();
        let entries = self.entries.clone();
        let entry = self.entry.clone();
        let supervisor = tokio::spawn(async move {
            let _work = work;
            let _lifecycle = lifecycle;
            let _instance_lifecycle = instance_lifecycle;
            let mut latch = ManagedOperationLatch::new(entries, entry);
            let worker = tokio::spawn(async move {
                match managed_mutation_admission(&managed_artifact_epoch) {
                    Ok(_mutation) => {
                        authority
                            .ensure_installed(
                                &identity,
                                &effects,
                                &plan,
                                &client,
                                before_target_effect,
                            )
                            .await
                    }
                    Err(error) => Err(ManagedInstallExecutionError::from_mutation(error, false)),
                }
            });
            let result = match worker.await {
                Ok(result) => result,
                Err(_) => {
                    return Err(ManagedInstallExecutionError::from_mutation(
                        ManagedMutationError::owner_stopped("install"),
                        false,
                    ));
                }
            };
            let indeterminate = result
                .as_ref()
                .err()
                .and_then(ManagedInstallExecutionError::mutation_error)
                .is_some_and(|error| matches!(error, ManagedMutationError::Indeterminate(_)));
            if !indeterminate && !effects_after.has_pending() {
                latch.disarm();
            }
            result
        });
        supervisor.await.unwrap_or_else(|_| {
            Err(ManagedInstallExecutionError::from_mutation(
                ManagedMutationError::owner_stopped("install"),
                false,
            ))
        })
    }

    pub(crate) async fn remove_managed(&self) -> Result<(), ManagedMutationError> {
        self.run_owned("remove", |authority, identity, effects, managed_artifact_epoch| async move {
            let _mutation = managed_mutation_admission(&managed_artifact_epoch)?;
            authority.remove_managed(&identity, &effects).await
        })
        .await
    }

    pub(crate) async fn rollback_managed(
        &self,
        snapshot_id: Option<&str>,
    ) -> Result<ManagedRollbackOutcome, ManagedMutationError> {
        let snapshot_id = snapshot_id.map(str::to_string);
        self.run_owned("rollback", move |authority, identity, effects, managed_artifact_epoch| async move {
            let _mutation = managed_mutation_admission(&managed_artifact_epoch)?;
            match snapshot_id {
                Some(snapshot_id) => {
                    authority
                        .rollback_managed_snapshot(&identity, &effects, &snapshot_id)
                        .await
                }
                None => authority.rollback_managed(&identity, &effects).await,
            }
        })
        .await
    }

    async fn run_owned<Output, Operation, OperationFuture>(
        &self,
        operation_name: &'static str,
        operation: Operation,
    ) -> Result<Output, ManagedMutationError>
    where
        Output: Send + 'static,
        Operation: FnOnce(
                ManagedCompositionAuthority,
                ManagedInstanceIdentity,
                ManagedInstanceEffectAuthority,
                super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
            ) -> OperationFuture
            + Send
            + 'static,
        OperationFuture:
            std::future::Future<Output = Result<Output, ManagedMutationError>> + Send + 'static,
    {
        let work = self.entry.work_gate.clone().lock_owned().await;
        let effects = self.entry.effects().clone();
        if self.entry.phase() != ManagedEntryPhase::Open || effects.require_settled().is_err() {
            publish_entry_phase(&self.entries, &self.entry, ManagedEntryPhase::Latched);
            return Err(ManagedMutationError::reconciliation_required(operation_name));
        }
        let lifecycle = self._lifecycle.clone();
        let instance_lifecycle = self.instance_lifecycle.retained();
        let authority = self.authority.clone();
        let identity = self.entry.identity.clone();
        let effects_after = effects.clone();
        let managed_artifact_epoch = self.managed_artifact_epoch.clone();
        let entries = self.entries.clone();
        let entry = self.entry.clone();
        let supervisor = tokio::spawn(async move {
            let _work = work;
            let _lifecycle = lifecycle;
            let _instance_lifecycle = instance_lifecycle;
            let mut latch = ManagedOperationLatch::new(entries, entry);
            let result = match tokio::spawn(operation(
                authority,
                identity,
                effects,
                managed_artifact_epoch,
            ))
            .await
            {
                Ok(result) => result,
                Err(_) => {
                    return Err(ManagedMutationError::owner_stopped(operation_name));
                }
            };
            if !matches!(result, Err(ManagedMutationError::Indeterminate(_)))
                && !effects_after.has_pending()
            {
                latch.disarm();
            }
            result
        });
        supervisor
            .await
            .unwrap_or_else(|_| Err(ManagedMutationError::owner_stopped(operation_name)))
    }
}

fn publish_entry_phase(
    entries: &Arc<Mutex<ManagedEntries>>,
    entry: &Arc<ManagedInstanceEntry>,
    phase: ManagedEntryPhase,
) {
    entry.store_phase(phase);
    let mut entries = entries
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(slot) = entries.get_mut(entry.identity.instance_id()) else {
        return;
    };
    if !slot
        .entry
        .upgrade()
        .is_some_and(|current| Arc::ptr_eq(&current, entry))
    {
        return;
    }
    slot.retained = (phase == ManagedEntryPhase::Latched).then(|| entry.clone());
}

fn managed_mutation_admission(
    managed_artifact_epoch: &super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
) -> Result<super::ManagedArtifactMutationAdmission, ManagedMutationError> {
    managed_artifact_epoch.admit().map_err(|error| {
        ManagedMutationError::Definite(axial_performance::InstallError::Io(io::Error::other(
            error.to_string(),
        )))
    })
}

pub(super) fn managed_authority_claim_error(
    error: io::Error,
) -> axial_performance::RulesRefreshError {
    axial_performance::RulesRefreshError::Cache(error)
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedCompositionAdmissionError, ManagedCompositionOwner, ManagedEntryPhase,
        ManagedMutationError,
    };
    use axial_performance::types::VersionFamily;
    use axial_performance::{
        CompositionPlan, CompositionTier, HardwareProfile, ManagedCompositionInstallPlan,
        PerformanceManager, PerformanceMode, ResolutionRequest,
    };
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
    use std::task::{Context, Poll};

    const INSTANCE_A: &str = "0000000000000001";
    const INSTANCE_B: &str = "0000000000000002";

    struct OwnerFixture {
        root: std::path::PathBuf,
        owner: Arc<ManagedCompositionOwner>,
        _root_session: Arc<axial_config::AppRootSession>,
        managed_artifact_epoch:
            super::super::managed_artifact_epoch::ManagedArtifactMutationEpochCoordinator,
        instance_lifecycle: super::super::instance_lifecycle::InstanceLifecycleGates,
        _cleanup: TestRootCleanup,
    }

    struct TestRootCleanup(std::path::PathBuf);

    impl Drop for TestRootCleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    impl OwnerFixture {
        fn new(name: &str) -> Self {
            Self::new_with_instances(
                name,
                [INSTANCE_A.to_string(), INSTANCE_B.to_string()],
            )
        }

        fn new_with_instances(
            name: &str,
            instance_ids: impl IntoIterator<Item = String>,
        ) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            let app_root = std::env::temp_dir().join(format!(
                "axial-managed-owner-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            let paths = axial_config::AppPaths::from_root(app_root.clone()).expect("app paths");
            let root = paths.instances_dir().to_path_buf();
            for instance_id in instance_ids {
                std::fs::create_dir_all(root.join(instance_id))
                    .expect("create managed owner instance directory");
            }
            let root_session = crate::state::test_root_session(&paths);
            let instances_directory = root_session
                .prepare_instances_directory()
                .expect("prepare managed owner instances directory");
            let manager = Arc::new(PerformanceManager::new().expect("performance manager"));
            let authority = manager
                .claim_managed_authority(instances_directory)
                .expect("claim managed authority");
            let instance_lifecycle =
                super::super::instance_lifecycle::InstanceLifecycleGates::default();
            let managed_artifact_epoch = super::super::managed_artifact_epoch::
                ManagedArtifactMutationEpochCoordinator::default();
            Self {
                root,
                owner: Arc::new(ManagedCompositionOwner::claim(
                    authority,
                    instance_lifecycle.clone(),
                    managed_artifact_epoch.clone(),
                )),
                _root_session: root_session,
                managed_artifact_epoch,
                instance_lifecycle,
                _cleanup: TestRootCleanup(app_root),
            }
        }

        fn mods_dir(&self, instance_id: &str) -> std::path::PathBuf {
            self.root.join(instance_id).join("mods")
        }

        async fn admit(
            &self,
            instance_id: &str,
        ) -> Result<super::AppManagedCompositionAdmission, ManagedCompositionAdmissionError>
        {
            self.admit_with_recovery(instance_id, true).await
        }

        async fn admit_with_recovery(
            &self,
            instance_id: &str,
            recovery_allowed: bool,
        ) -> Result<super::AppManagedCompositionAdmission, ManagedCompositionAdmissionError>
        {
            let lifecycle = super::super::InstanceLifecycleLease::bind(
                instance_id,
                self.instance_lifecycle.clone(),
                self.instance_lifecycle.acquire(instance_id).await,
            );
            self.owner
                .admit(instance_id, lifecycle, recovery_allowed)
                .await
        }

        async fn retire(
            &self,
            instance_id: &str,
        ) -> Result<super::ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
            let lifecycle = super::super::InstanceLifecycleLease::bind(
                instance_id,
                self.instance_lifecycle.clone(),
                self.instance_lifecycle.acquire(instance_id).await,
            );
            self.owner.retire(instance_id, lifecycle).await
        }

        async fn retire_existing(
            &self,
            instance_id: &str,
        ) -> Result<Option<super::ManagedCompositionRetirement>, ManagedCompositionAdmissionError>
        {
            let lifecycle = super::super::InstanceLifecycleLease::bind(
                instance_id,
                self.instance_lifecycle.clone(),
                self.instance_lifecycle.acquire(instance_id).await,
            );
            self.owner.retire_existing(instance_id, lifecycle).await
        }

        fn artifact_epoch(&self) -> u64 {
            self.managed_artifact_epoch
                .current()
                .expect("managed artifact epoch")
                .value()
        }
    }

    fn poll_once<Output>(future: Pin<&mut impl Future<Output = Output>>) -> Poll<Output> {
        let waker = futures_util::task::noop_waker();
        let mut context = Context::from_waker(&waker);
        future.poll(&mut context)
    }

    #[tokio::test]
    async fn same_instance_admission_waits_for_the_current_guard() {
        let fixture = OwnerFixture::new("same-instance-gate");
        let first = fixture.admit(INSTANCE_A).await.expect("first admission");
        let mut second = Box::pin(fixture.admit(INSTANCE_A));

        assert!(matches!(poll_once(second.as_mut()), Poll::Pending));
        drop(first);

        second.await.expect("second admission after guard release");
    }

    #[tokio::test]
    async fn different_instance_admission_progresses_independently() {
        let fixture = OwnerFixture::new("different-instance-gates");
        let _first = fixture.admit(INSTANCE_A).await.expect("first admission");
        let mut second = Box::pin(fixture.admit(INSTANCE_B));

        let second = match poll_once(second.as_mut()) {
            Poll::Ready(result) => result.expect("different instance admission"),
            Poll::Pending => panic!("different instance admission must not share the gate"),
        };
        drop(second);
    }

    #[tokio::test]
    async fn foreign_instance_lifecycle_authority_is_rejected() {
        let fixture = OwnerFixture::new("foreign-instance-lifecycle");
        let foreign = super::super::instance_lifecycle::InstanceLifecycleGates::default();
        let admission_lease = super::super::InstanceLifecycleLease::bind(
            INSTANCE_A,
            foreign.clone(),
            foreign.acquire(INSTANCE_A).await,
        );
        assert!(matches!(
            fixture
                .owner
                .admit(INSTANCE_A, admission_lease, true)
                .await,
            Err(ManagedCompositionAdmissionError::ForeignLifecycleAuthority)
        ));

        let retirement_lease = super::super::InstanceLifecycleLease::bind(
            INSTANCE_A,
            foreign.clone(),
            foreign.acquire(INSTANCE_A).await,
        );
        assert!(matches!(
            fixture.owner.retire(INSTANCE_A, retirement_lease).await,
            Err(ManagedCompositionAdmissionError::ForeignLifecycleAuthority)
        ));
    }

    #[tokio::test]
    async fn close_drains_admitted_guards_and_rejects_late_admission() {
        let fixture = OwnerFixture::new("close-drain");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let mut close = Box::pin(fixture.owner.close());

        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));
        assert!(matches!(
            fixture.admit(INSTANCE_B).await,
            Err(ManagedCompositionAdmissionError::Closed)
        ));
        drop(admitted);

        close.await.expect("close after admitted guard drains");
    }

    #[tokio::test]
    async fn queued_close_does_not_block_work_owned_by_an_existing_admission() {
        let fixture = OwnerFixture::new("close-queued-before-work");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let mut close = Box::pin(fixture.owner.close());
        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));

        admitted
            .inspect(None)
            .await
            .expect("existing admission remains usable while close is queued");
        drop(admitted);

        close.await.expect("close after admitted work completes");
    }

    #[tokio::test]
    async fn queued_close_does_not_block_binding_owned_by_an_existing_admission() {
        let fixture = OwnerFixture::new("close-queued-during-bind");
        let entry = fixture.owner.entry(INSTANCE_A).expect("managed entry");
        let work = entry.work_gate.clone().lock_owned().await;
        let mut admission = Box::pin(fixture.admit(INSTANCE_A));
        assert!(matches!(poll_once(admission.as_mut()), Poll::Pending));
        let mut close = Box::pin(fixture.owner.close());
        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));

        drop(work);
        let admitted = tokio::time::timeout(std::time::Duration::from_secs(1), admission)
            .await
            .expect("binding must not queue a nested lifecycle read")
            .expect("admission after binding");
        drop(admitted);

        close.await.expect("close after binding completes");
    }

    #[tokio::test]
    async fn canceled_waiter_keeps_operation_ownership_until_the_worker_finishes() {
        let fixture = OwnerFixture::new("canceled-waiter-ownership");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let entry = admitted.entry.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(admitted.run_owned::<(), _, _>(
            "canceled_waiter_test",
            move |_, _, _, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Ok(())
            },
        ));
        assert!(matches!(poll_once(operation.as_mut()), Poll::Pending));
        started_rx.await.expect("worker started");

        drop(operation);
        drop(admitted);
        assert!(entry.work_gate.clone().try_lock_owned().is_err());
        assert!(fixture.instance_lifecycle.is_held(INSTANCE_A).await);
        let mut close = Box::pin(fixture.owner.close());
        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));

        release_tx.send(()).expect("release worker");
        close.await.expect("close after the owned worker finishes");
    }

    #[tokio::test]
    async fn canceled_waiter_worker_panic_latches_the_exact_entry() {
        let fixture = OwnerFixture::new("canceled-waiter-panic");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let entry = admitted.entry.clone();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut operation = Box::pin(admitted.run_owned::<(), _, _>(
            "canceled_waiter_panic_test",
            move |_, _, _, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                panic!("injected managed operation panic")
            },
        ));
        assert!(matches!(poll_once(operation.as_mut()), Poll::Pending));
        started_rx.await.expect("worker started");

        drop(operation);
        drop(admitted);
        release_tx.send(()).expect("release worker");
        let work = tokio::time::timeout(
            std::time::Duration::from_secs(1),
            entry.work_gate.clone().lock_owned(),
        )
        .await
        .expect("panicked worker must release the work gate");
        drop(work);

        assert_eq!(entry.phase(), ManagedEntryPhase::Latched);
        let retained = fixture
            .owner
            .entries
            .lock()
            .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
            .get(INSTANCE_A)
            .and_then(|slot| slot.retained.as_ref())
            .cloned()
            .expect("panicked worker retains its exact entry");
        assert!(Arc::ptr_eq(&retained, &entry));
        drop(retained);
        fixture
            .owner
            .close()
            .await
            .expect("clean panic latch remains recoverable");
    }

    #[tokio::test]
    async fn latched_admission_refuses_a_second_operation() {
        let fixture = OwnerFixture::new("latched-admission-reuse");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let first = admitted
            .run_owned::<(), _, _>("first_test_operation", |_, _, _, _| async move {
                Err(ManagedMutationError::reconciliation_required(
                    "injected_test_failure",
                ))
            })
            .await;
        assert!(matches!(first, Err(ManagedMutationError::Indeterminate(_))));

        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_by_operation = invoked.clone();
        let second = admitted
            .run_owned::<(), _, _>("second_test_operation", move |_, _, _, _| async move {
                invoked_by_operation.store(true, Ordering::SeqCst);
                Ok(())
            })
            .await;
        assert!(matches!(second, Err(ManagedMutationError::Indeterminate(_))));
        assert!(!invoked.load(Ordering::SeqCst));

        drop(admitted);
        fixture
            .owner
            .close()
            .await
            .expect("synthetic clean latch remains recoverable");
    }

    #[tokio::test]
    async fn operation_queued_behind_a_latching_worker_never_starts() {
        let fixture = OwnerFixture::new("queued-operation-after-latch");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut first = Box::pin(admitted.run_owned::<(), _, _>(
            "first_queued_test_operation",
            move |_, _, _, _| async move {
                let _ = started_tx.send(());
                let _ = release_rx.await;
                Err(ManagedMutationError::reconciliation_required(
                    "injected_queued_failure",
                ))
            },
        ));
        assert!(matches!(poll_once(first.as_mut()), Poll::Pending));
        started_rx.await.expect("first worker started");

        let invoked = Arc::new(AtomicBool::new(false));
        let invoked_by_operation = invoked.clone();
        let mut second = Box::pin(admitted.run_owned::<(), _, _>(
            "second_queued_test_operation",
            move |_, _, _, _| async move {
                invoked_by_operation.store(true, Ordering::SeqCst);
                Ok(())
            },
        ));
        assert!(matches!(poll_once(second.as_mut()), Poll::Pending));

        release_tx.send(()).expect("release first worker");
        assert!(matches!(
            first.await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        assert!(matches!(
            second.await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        assert!(!invoked.load(Ordering::SeqCst));

        drop(admitted);
        fixture
            .owner
            .close()
            .await
            .expect("queued synthetic latch remains recoverable");
    }

    #[tokio::test]
    async fn existing_only_retirement_does_not_bind_an_unopened_instance() {
        let fixture = OwnerFixture::new("existing-only-retirement");
        assert!(!fixture.mods_dir(INSTANCE_A).exists());

        let retirement = fixture
            .retire_existing(INSTANCE_A)
            .await
            .expect("inspect existing managed owner");

        assert!(retirement.is_none());
        assert!(!fixture.mods_dir(INSTANCE_A).exists());
        assert!(
            fixture
                .owner
                .entries
                .lock()
                .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn existing_only_retirement_settles_an_already_bound_entry() {
        let fixture = OwnerFixture::new("existing-only-bound-retirement");
        let sentinel = fixture.root.join(INSTANCE_A).join("keep-files.txt");
        std::fs::write(&sentinel, b"preserved").expect("seed keep-files sentinel");
        let admission = fixture.admit(INSTANCE_A).await.expect("bind managed entry");
        drop(admission);

        let retirement = fixture
            .retire_existing(INSTANCE_A)
            .await
            .expect("retire existing managed entry")
            .unwrap_or_else(|| panic!("bound entry requires retirement"));
        retirement.commit();

        assert_eq!(
            std::fs::read(&sentinel).expect("read keep-files sentinel"),
            b"preserved"
        );
        assert!(
            fixture
                .owner
                .entries
                .lock()
                .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn retirement_rolls_back_uncommitted_and_removes_committed_entry() {
        let uncommitted = OwnerFixture::new("retirement-rollback");
        let retirement = uncommitted
            .retire(INSTANCE_A)
            .await
            .expect("retire instance");
        let mut waiting = Box::pin(uncommitted.admit(INSTANCE_A));
        assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
        drop(retirement);
        waiting
            .await
            .expect("uncommitted retirement clears tombstone");

        let committed = OwnerFixture::new("retirement-commit");
        committed
            .retire(INSTANCE_A)
            .await
            .expect("retire instance")
            .commit();
        assert!(
            committed
                .owner
                .entries
                .lock()
                .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
                .is_empty()
        );
        committed
            .admit(INSTANCE_A)
            .await
            .expect("a recreated identity gets a fresh owner entry");
    }

    #[tokio::test]
    async fn committed_retirement_does_not_remove_a_stale_replacement_entry() {
        let fixture = OwnerFixture::new("retirement-stale-owner");
        let retirement = fixture
            .retire(INSTANCE_A)
            .await
            .expect("retire original entry");
        let original = retirement.entry.clone();
        fixture
            .owner
            .entries
            .lock()
            .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
            .remove(INSTANCE_A);
        let replacement = fixture.owner.entry(INSTANCE_A).expect("replacement entry");
        assert!(!Arc::ptr_eq(&original, &replacement));

        retirement.commit();

        let retained = fixture
            .owner
            .entries
            .lock()
            .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
            .get(INSTANCE_A)
            .and_then(|slot| slot.entry.upgrade())
            .expect("stale commit must retain replacement");
        assert!(Arc::ptr_eq(&retained, &replacement));
        assert_eq!(retained.phase(), ManagedEntryPhase::Open);
    }

    #[tokio::test]
    async fn duplicate_retirement_owner_cannot_retain_or_remove_an_entry() {
        let fixture = OwnerFixture::new("retirement-duplicate-owner");
        let retirement = fixture
            .retire(INSTANCE_A)
            .await
            .expect("retire exact entry");
        let duplicate_lifecycle = retirement._instance_lifecycle.retained();
        let mut duplicate = Box::pin(
            fixture
                .owner
                .retire(INSTANCE_A, duplicate_lifecycle),
        );
        assert!(matches!(poll_once(duplicate.as_mut()), Poll::Pending));

        retirement.commit();
        assert!(matches!(
            duplicate.await,
            Err(ManagedCompositionAdmissionError::Retired)
        ));
        assert!(
            fixture
                .owner
                .entries
                .lock()
                .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
                .is_empty()
        );
    }

    #[tokio::test]
    async fn committed_retirement_churn_keeps_the_owner_map_bounded() {
        let fixture = OwnerFixture::new("retirement-churn");
        for _ in 0..512 {
            fixture
                .retire(INSTANCE_A)
                .await
                .expect("reserve churn retirement")
                .commit();
            assert!(
                fixture
                    .owner
                    .entries
                    .lock()
                    .expect(super::MANAGED_OWNER_LOCK_INVARIANT)
                    .is_empty()
            );
        }
    }

    #[tokio::test]
    async fn sequential_clean_instances_release_effect_owner_capacity() {
        let instance_ids = (1_u64..=300)
            .map(|index| format!("{index:016x}"))
            .collect::<Vec<_>>();
        let fixture = OwnerFixture::new_with_instances("clean-owner-eviction", instance_ids.clone());

        for instance_id in instance_ids {
            drop(
                fixture
                    .admit(&instance_id)
                    .await
                    .expect("admit sequential clean instance"),
            );
        }

        let entries = fixture
            .owner
            .entries
            .lock()
            .expect(super::MANAGED_OWNER_LOCK_INVARIANT);
        assert!(entries.len() <= 1);
        assert!(entries.values().all(|slot| slot.entry.strong_count() == 0));
    }

    #[tokio::test]
    async fn healthy_inspection_and_resolution_leave_the_artifact_epoch_unchanged() {
        let fixture = OwnerFixture::new("healthy-inspection-epoch");
        std::fs::create_dir_all(fixture.mods_dir(INSTANCE_A))
            .expect("create healthy instance mods directory");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let initial_epoch = fixture.artifact_epoch();

        admitted.inspect(None).await.expect("healthy inspection");
        admitted
            .resolve_and_inspect(ResolutionRequest {
                game_version: "1.21.1".to_string(),
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                hardware: HardwareProfile::default(),
                installed_mods: Vec::new(),
            })
            .await
            .expect("healthy resolved inspection");

        assert_eq!(fixture.artifact_epoch(), initial_epoch);
    }

    #[tokio::test]
    async fn effectful_inspection_advances_the_artifact_epoch_once() {
        let fixture = OwnerFixture::new("effectful-inspection-epoch");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        let staged = mods_dir.join(".axial-lock.json.new.tmp");
        let plan = ManagedCompositionInstallPlan::seal(
            CompositionPlan {
                composition_id: "core".to_string(),
                family: VersionFamily::F,
                loader: "fabric".to_string(),
                mode: PerformanceMode::Managed,
                tier: CompositionTier::Core,
                mods: Vec::new(),
                jvm_preset: String::new(),
                warnings: Vec::new(),
                fallback_reason: String::new(),
            },
            "1.21.1",
            "fabric",
            Vec::new(),
            Vec::new(),
        )
        .expect("seal staged state fixture");
        std::fs::write(
            &staged,
            serde_json::to_vec_pretty(&serde_json::json!({
                "schema_version": 2,
                "state": {
                    "composition_id": "core",
                    "family": "F",
                    "tier": "core",
                    "game_version": "1.21.1",
                    "loader": "fabric",
                    "graph_sha512": plan.graph_digest(),
                    "dependency_edges": [],
                    "installed_mods": [],
                    "installed_at": "2026-07-17T00:00:00Z"
                }
            }))
            .expect("serialize staged state"),
        )
        .expect("seed interrupted state publication");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        let initial_epoch = fixture.artifact_epoch();

        admitted
            .inspect(None)
            .await
            .expect("recover interrupted publication");

        assert_eq!(fixture.artifact_epoch(), initial_epoch + 1);
        assert!(!staged.exists());
        assert!(mods_dir.join(".axial-lock.json").is_file());

        admitted
            .inspect(None)
            .await
            .expect("settled follow-up inspection");
        assert_eq!(fixture.artifact_epoch(), initial_epoch + 1);
    }

    #[tokio::test]
    async fn indeterminate_inspection_latches_admission_and_close() {
        let fixture = OwnerFixture::new("inspection-latch");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        std::fs::write(mods_dir.join(".axial-lock.json.new.tmp"), b"not-json")
            .expect("seed ambiguous publication stage");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");

        assert!(matches!(
            admitted.inspect(None).await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        assert!(matches!(
            fixture.admit(INSTANCE_A).await,
            Err(ManagedCompositionAdmissionError::RecoveryFailed)
        ));
        assert!(fixture.owner.close().await.is_err());
        assert!(matches!(
            fixture.admit(INSTANCE_B).await,
            Err(ManagedCompositionAdmissionError::Closed)
        ));
        std::fs::remove_file(mods_dir.join(".axial-lock.json.new.tmp"))
            .expect("repair ambiguous publication stage");
        fixture
            .owner
            .close()
            .await
            .expect("close retries exact recovery");
        fixture.owner.close().await.expect("close is idempotent");
    }

    #[tokio::test]
    async fn later_admission_recovers_latched_entry_before_continuing() {
        let fixture = OwnerFixture::new("admission-recovery");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        let staged = mods_dir.join(".axial-lock.json.new.tmp");
        std::fs::write(&staged, b"not-json").expect("seed ambiguous publication stage");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        assert!(matches!(
            admitted.inspect(None).await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        std::fs::remove_file(staged).expect("repair publication stage");

        fixture
            .admit(INSTANCE_A)
            .await
            .expect("exact recovery reopens admission");
    }

    #[tokio::test]
    async fn latched_admission_does_not_recover_while_instance_is_running() {
        let fixture = OwnerFixture::new("active-session-blocks-recovery");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        let staged = mods_dir.join(".axial-lock.json.new.tmp");
        std::fs::write(&staged, b"not-json").expect("seed ambiguous publication stage");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        assert!(matches!(
            admitted.inspect(None).await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        std::fs::remove_file(staged).expect("repair publication stage");

        assert!(matches!(
            fixture.admit_with_recovery(INSTANCE_A, false).await,
            Err(ManagedCompositionAdmissionError::RecoveryBlockedByActiveSession)
        ));
        assert_eq!(
            fixture
                .owner
                .entry(INSTANCE_A)
                .expect("latched entry")
                .phase(),
            ManagedEntryPhase::Latched
        );

        fixture
            .admit_with_recovery(INSTANCE_A, true)
            .await
            .expect("recovery resumes after the instance stops");
        assert_eq!(
            fixture
                .owner
                .entry(INSTANCE_A)
                .expect("recovered entry")
                .phase(),
            ManagedEntryPhase::Open
        );
    }

    #[tokio::test]
    async fn close_recovers_all_entries_and_retries_only_remaining_latches() {
        let fixture = OwnerFixture::new("partial-close-retry");
        let staged_a = fixture
            .mods_dir(INSTANCE_A)
            .join(".axial-lock.json.new.tmp");
        let staged_b = fixture
            .mods_dir(INSTANCE_B)
            .join(".axial-lock.json.new.tmp");
        std::fs::create_dir_all(staged_a.parent().expect("instance A mods parent"))
            .expect("create instance A mods");
        std::fs::create_dir_all(staged_b.parent().expect("instance B mods parent"))
            .expect("create instance B mods");
        std::fs::write(&staged_a, b"not-json").expect("seed instance A ambiguity");
        std::fs::write(&staged_b, b"not-json").expect("seed instance B ambiguity");
        for instance_id in [INSTANCE_A, INSTANCE_B] {
            let admitted = fixture.admit(instance_id).await.expect("admission");
            assert!(matches!(
                admitted.inspect(None).await,
                Err(ManagedMutationError::Indeterminate(_))
            ));
        }
        std::fs::remove_file(&staged_a).expect("repair instance A");

        assert!(fixture.owner.close().await.is_err());
        assert_eq!(
            fixture
                .owner
                .entry(INSTANCE_A)
                .expect("instance A entry")
                .phase(),
            ManagedEntryPhase::Open
        );
        assert_eq!(
            fixture
                .owner
                .entry(INSTANCE_B)
                .expect("instance B entry")
                .phase(),
            ManagedEntryPhase::Latched
        );

        std::fs::remove_file(staged_b).expect("repair instance B");
        fixture.owner.close().await.expect("retry remaining latch");
        fixture
            .owner
            .close()
            .await
            .expect("closed owner is idempotent");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn canceled_recovery_waiter_does_not_release_owned_guards() {
        let fixture = OwnerFixture::new("canceled-recovery-waiter");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        let staged = mods_dir.join(".axial-lock.json.new.tmp");
        std::fs::write(&staged, b"not-json").expect("seed ambiguous publication stage");
        let admitted = fixture.admit(INSTANCE_A).await.expect("admission");
        assert!(matches!(
            admitted.inspect(None).await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        std::fs::remove_file(staged).expect("repair publication stage");

        let mut recovery = Box::pin(fixture.admit(INSTANCE_A));
        assert!(matches!(poll_once(recovery.as_mut()), Poll::Pending));
        drop(recovery);
        let mut close = Box::pin(fixture.owner.close());
        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));

        tokio::task::yield_now().await;
        close
            .await
            .expect("owned recovery completes after waiter cancellation");
    }

    #[tokio::test]
    async fn admission_accepts_only_canonical_instance_ids() {
        let fixture = OwnerFixture::new("canonical-identity");

        fixture
            .admit(INSTANCE_A)
            .await
            .expect("canonical instance id");
        assert!(matches!(
            fixture.admit("000000000000000A").await,
            Err(ManagedCompositionAdmissionError::Identity(_))
        ));
        assert!(matches!(
            fixture.admit("../00000000000001").await,
            Err(ManagedCompositionAdmissionError::Identity(_))
        ));
    }
}
