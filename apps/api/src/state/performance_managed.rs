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
        if entry.retired.load(Ordering::Acquire) {
            return Err(ManagedCompositionAdmissionError::Retired);
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

#[cfg(test)]
mod tests {
    use super::{ManagedCompositionAdmissionError, ManagedCompositionOwner, ManagedMutationError};
    use axial_performance::PerformanceManager;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::{Context, Poll};

    const INSTANCE_A: &str = "0000000000000001";
    const INSTANCE_B: &str = "0000000000000002";

    struct OwnerFixture {
        root: std::path::PathBuf,
        owner: Arc<ManagedCompositionOwner>,
    }

    impl OwnerFixture {
        fn new(name: &str) -> Self {
            static NEXT_ID: AtomicU64 = AtomicU64::new(1);
            let root = std::env::temp_dir().join(format!(
                "axial-managed-owner-{name}-{}-{}",
                std::process::id(),
                NEXT_ID.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&root).expect("create managed owner root");
            let manager = Arc::new(PerformanceManager::new().expect("performance manager"));
            let authority = manager
                .claim_managed_authority(&root)
                .expect("claim managed authority");
            Self {
                root,
                owner: Arc::new(ManagedCompositionOwner::claim(authority)),
            }
        }

        fn mods_dir(&self, instance_id: &str) -> std::path::PathBuf {
            self.root.join(instance_id).join("mods")
        }
    }

    impl Drop for OwnerFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
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
        let first = fixture
            .owner
            .admit(INSTANCE_A)
            .await
            .expect("first admission");
        let mut second = Box::pin(fixture.owner.admit(INSTANCE_A));

        assert!(matches!(poll_once(second.as_mut()), Poll::Pending));
        drop(first);

        second.await.expect("second admission after guard release");
    }

    #[tokio::test]
    async fn different_instance_admission_progresses_independently() {
        let fixture = OwnerFixture::new("different-instance-gates");
        let _first = fixture
            .owner
            .admit(INSTANCE_A)
            .await
            .expect("first admission");
        let mut second = Box::pin(fixture.owner.admit(INSTANCE_B));

        let second = match poll_once(second.as_mut()) {
            Poll::Ready(result) => result.expect("different instance admission"),
            Poll::Pending => panic!("different instance admission must not share the gate"),
        };
        drop(second);
    }

    #[tokio::test]
    async fn close_drains_admitted_guards_and_rejects_late_admission() {
        let fixture = OwnerFixture::new("close-drain");
        let admitted = fixture.owner.admit(INSTANCE_A).await.expect("admission");
        let mut close = Box::pin(fixture.owner.close());

        assert!(matches!(poll_once(close.as_mut()), Poll::Pending));
        assert!(matches!(
            fixture.owner.admit(INSTANCE_B).await,
            Err(ManagedCompositionAdmissionError::Closed)
        ));
        drop(admitted);

        close.await.expect("close after admitted guard drains");
    }

    #[tokio::test]
    async fn retirement_rolls_back_uncommitted_and_preserves_committed_tombstone() {
        let uncommitted = OwnerFixture::new("retirement-rollback");
        let retirement = uncommitted
            .owner
            .retire(INSTANCE_A)
            .await
            .expect("retire instance");
        let mut waiting = Box::pin(uncommitted.owner.admit(INSTANCE_A));
        assert!(matches!(poll_once(waiting.as_mut()), Poll::Pending));
        drop(retirement);
        waiting
            .await
            .expect("uncommitted retirement clears tombstone");

        let committed = OwnerFixture::new("retirement-commit");
        committed
            .owner
            .retire(INSTANCE_A)
            .await
            .expect("retire instance")
            .commit();
        assert!(matches!(
            committed.owner.admit(INSTANCE_A).await,
            Err(ManagedCompositionAdmissionError::Retired)
        ));
        assert!(matches!(
            committed.owner.retire(INSTANCE_A).await,
            Err(ManagedCompositionAdmissionError::Retired)
        ));
    }

    #[tokio::test]
    async fn indeterminate_inspection_latches_admission_and_close() {
        let fixture = OwnerFixture::new("inspection-latch");
        let mods_dir = fixture.mods_dir(INSTANCE_A);
        std::fs::create_dir_all(&mods_dir).expect("create instance mods directory");
        std::fs::write(mods_dir.join(".axial-lock.json.new.tmp"), b"not-json")
            .expect("seed ambiguous publication stage");
        let admitted = fixture.owner.admit(INSTANCE_A).await.expect("admission");

        assert!(matches!(
            admitted.inspect(None).await,
            Err(ManagedMutationError::Indeterminate(_))
        ));
        drop(admitted);
        assert!(matches!(
            fixture.owner.admit(INSTANCE_A).await,
            Err(ManagedCompositionAdmissionError::ReconciliationRequired)
        ));
        assert!(fixture.owner.close().await.is_err());
        assert!(matches!(
            fixture.owner.admit(INSTANCE_B).await,
            Err(ManagedCompositionAdmissionError::Closed)
        ));
    }

    #[tokio::test]
    async fn admission_accepts_only_canonical_instance_ids() {
        let fixture = OwnerFixture::new("canonical-identity");

        fixture
            .owner
            .admit(INSTANCE_A)
            .await
            .expect("canonical instance id");
        assert!(matches!(
            fixture.owner.admit("000000000000000A").await,
            Err(ManagedCompositionAdmissionError::Identity(_))
        ));
        assert!(matches!(
            fixture.owner.admit("../00000000000001").await,
            Err(ManagedCompositionAdmissionError::Identity(_))
        ));
    }
}
