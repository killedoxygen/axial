use axial_performance::{
    CompositionPlan, CompositionState, ManagedCompositionAuthority, ManagedCompositionInspection,
    ManagedInstanceIdentity, ManagedMutationError, ManagedResolvedInspection, ResolutionRequest,
};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, RwLock as AsyncRwLock,
};

const MANAGED_OWNER_LOCK_INVARIANT: &str =
    "managed composition owner lock poisoned; admission state may be inconsistent";

struct ManagedInstanceEntry {
    identity: ManagedInstanceIdentity,
    gate: Arc<AsyncMutex<()>>,
    reconciliation_required: AtomicBool,
    retired: AtomicBool,
}

pub(super) struct ManagedCompositionOwner {
    authority: ManagedCompositionAuthority,
    entries: Mutex<HashMap<String, Arc<ManagedInstanceEntry>>>,
    lifecycle: Arc<AsyncRwLock<()>>,
    admission_closed: AtomicBool,
}

pub(crate) struct ManagedCompositionAdmission {
    authority: ManagedCompositionAuthority,
    entry: Arc<ManagedInstanceEntry>,
    _lifecycle: OwnedRwLockReadGuard<()>,
    _gate: OwnedMutexGuard<()>,
}

pub(crate) struct ManagedCompositionRetirement {
    entry: Arc<ManagedInstanceEntry>,
    _lifecycle: OwnedRwLockReadGuard<()>,
    _gate: OwnedMutexGuard<()>,
    committed: bool,
}

impl ManagedCompositionRetirement {
    pub(crate) fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for ManagedCompositionRetirement {
    fn drop(&mut self) {
        if !self.committed {
            self.entry.retired.store(false, Ordering::Release);
        }
    }
}

pub(crate) struct AppManagedCompositionAdmission {
    managed: ManagedCompositionAdmission,
    _instance_lifecycle: Option<OwnedMutexGuard<()>>,
}

impl AppManagedCompositionAdmission {
    pub(super) fn bind(
        managed: ManagedCompositionAdmission,
        instance_lifecycle: Option<OwnedMutexGuard<()>>,
    ) -> Self {
        Self {
            managed,
            _instance_lifecycle: instance_lifecycle,
        }
    }
}

impl std::ops::Deref for AppManagedCompositionAdmission {
    type Target = ManagedCompositionAdmission;

    fn deref(&self) -> &Self::Target {
        &self.managed
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedCompositionAdmissionError {
    #[error("managed composition admission is closed")]
    Closed,
    #[error("managed composition reconciliation is required for this instance")]
    ReconciliationRequired,
    #[error("managed composition identity is retired")]
    Retired,
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
    #[error("{0}")]
    Owner(#[from] ManagedCompositionAdmissionError),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedInspectionError {
    #[error("{0}")]
    Admission(#[from] ManagedInstanceAdmissionError),
    #[error("{0}")]
    Operation(#[from] ManagedMutationError),
    #[error("managed composition inspection owner stopped before reporting completion")]
    OwnerStopped,
}

#[derive(Debug, thiserror::Error)]
#[error("managed composition shutdown is blocked by reconciliation-required instances")]
pub(crate) struct ManagedCompositionCloseError;

impl ManagedCompositionOwner {
    pub(super) fn claim(authority: ManagedCompositionAuthority) -> Self {
        Self {
            authority,
            entries: Mutex::new(HashMap::new()),
            lifecycle: Arc::new(AsyncRwLock::new(())),
            admission_closed: AtomicBool::new(false),
        }
    }

    pub(super) async fn admit(
        &self,
        instance_id: &str,
    ) -> Result<ManagedCompositionAdmission, ManagedCompositionAdmissionError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let lifecycle = self.lifecycle.clone().read_owned().await;
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let entry = self.entry(instance_id)?;
        let gate = entry.gate.clone().lock_owned().await;
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        if entry.reconciliation_required.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::ReconciliationRequired);
        }
        if entry.retired.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Retired);
        }
        Ok(ManagedCompositionAdmission {
            authority: self.authority.clone(),
            entry,
            _lifecycle: lifecycle,
            _gate: gate,
        })
    }

    pub(super) async fn retire(
        &self,
        instance_id: &str,
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let lifecycle = self.lifecycle.clone().read_owned().await;
        let entry = self.entry(instance_id)?;
        let gate = entry.gate.clone().lock_owned().await;
        if self.admission_closed.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        if entry.reconciliation_required.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::ReconciliationRequired);
        }
        entry.retired.store(true, Ordering::Release);
        Ok(ManagedCompositionRetirement {
            entry,
            _lifecycle: lifecycle,
            _gate: gate,
            committed: false,
        })
    }

    pub(super) async fn close(&self) -> Result<(), ManagedCompositionCloseError> {
        self.admission_closed.store(true, Ordering::Release);
        let _drained = self.lifecycle.write().await;
        let mut entries = self
            .entries
            .lock()
            .expect(MANAGED_OWNER_LOCK_INVARIANT)
            .values()
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.identity
                .instance_id()
                .cmp(right.identity.instance_id())
        });
        if entries
            .iter()
            .any(|entry| entry.reconciliation_required.load(Ordering::Acquire))
        {
            return Err(ManagedCompositionCloseError);
        }
        Ok(())
    }

    fn entry(
        &self,
        instance_id: &str,
    ) -> Result<Arc<ManagedInstanceEntry>, ManagedCompositionAdmissionError> {
        let mut entries = self.entries.lock().expect(MANAGED_OWNER_LOCK_INVARIANT);
        if let Some(entry) = entries.get(instance_id) {
            return Ok(entry.clone());
        }
        let identity = self.authority.identify(instance_id)?;
        let entry = Arc::new(ManagedInstanceEntry {
            identity,
            gate: Arc::new(AsyncMutex::new(())),
            reconciliation_required: AtomicBool::new(false),
            retired: AtomicBool::new(false),
        });
        entries.insert(instance_id.to_string(), entry.clone());
        Ok(entry)
    }
}

impl ManagedCompositionAdmission {
    pub(crate) async fn inspect(
        &self,
        plan: Option<&CompositionPlan>,
    ) -> Result<ManagedCompositionInspection, ManagedMutationError> {
        let result = self.authority.inspect(&self.entry.identity, plan).await;
        self.latch_indeterminate(&result);
        result
    }

    pub(crate) async fn resolve_and_inspect(
        &self,
        request: ResolutionRequest,
    ) -> Result<ManagedResolvedInspection, ManagedMutationError> {
        let result = self
            .authority
            .resolve_and_inspect(&self.entry.identity, request)
            .await;
        self.latch_indeterminate(&result);
        result
    }

    pub(crate) async fn ensure_installed(
        &self,
        plan: &CompositionPlan,
        game_version: &str,
    ) -> Result<CompositionState, ManagedMutationError> {
        let result = self
            .authority
            .ensure_installed(&self.entry.identity, plan, game_version)
            .await;
        self.latch_indeterminate(&result);
        result
    }

    pub(crate) async fn remove_managed(&self) -> Result<(), ManagedMutationError> {
        let result = self.authority.remove_managed(&self.entry.identity).await;
        self.latch_indeterminate(&result);
        result
    }

    pub(crate) async fn rollback_managed(
        &self,
        snapshot_id: Option<&str>,
    ) -> Result<CompositionState, ManagedMutationError> {
        let result = match snapshot_id {
            Some(snapshot_id) => {
                self.authority
                    .rollback_managed_snapshot(&self.entry.identity, snapshot_id)
                    .await
            }
            None => self.authority.rollback_managed(&self.entry.identity).await,
        };
        self.latch_indeterminate(&result);
        result
    }

    fn latch_indeterminate<T>(&self, result: &Result<T, ManagedMutationError>) {
        if matches!(result, Err(ManagedMutationError::Indeterminate(_))) {
            self.entry
                .reconciliation_required
                .store(true, Ordering::Release);
        }
    }
}

pub(super) fn managed_authority_claim_error(
    error: io::Error,
) -> axial_performance::RulesRefreshError {
    axial_performance::RulesRefreshError::Cache(error)
}
