use axial_performance::{
    CompositionPlan, CompositionState, ManagedCompositionAuthority, ManagedCompositionInspection,
    ManagedInstanceIdentity, ManagedMutationError, ManagedResolvedInspection, ResolutionRequest,
};
use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{
    Mutex as AsyncMutex, OwnedMutexGuard, OwnedRwLockReadGuard, RwLock as AsyncRwLock,
};

const MANAGED_OWNER_LOCK_INVARIANT: &str =
    "managed composition owner lock poisoned; admission state may be inconsistent";

struct ManagedInstanceEntry {
    identity: ManagedInstanceIdentity,
    gate: Arc<AsyncMutex<()>>,
    phase: AtomicU8,
}

pub(super) struct ManagedCompositionOwner {
    authority: ManagedCompositionAuthority,
    entries: Mutex<HashMap<String, Arc<ManagedInstanceEntry>>>,
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
            self.entry.store_phase(ManagedEntryPhase::Open);
        }
    }
}

pub(crate) struct AppManagedCompositionAdmission {
    managed: ManagedCompositionAdmission,
    _instance_lifecycle: OwnedMutexGuard<()>,
}

impl AppManagedCompositionAdmission {
    pub(super) fn bind(
        managed: ManagedCompositionAdmission,
        instance_lifecycle: OwnedMutexGuard<()>,
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
    #[error("managed composition exact recovery could not prove a clean state")]
    RecoveryFailed,
    #[error("managed composition recovery is blocked while the instance is running")]
    RecoveryBlockedByActiveSession,
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
    pub(super) fn claim(
        authority: ManagedCompositionAuthority,
        instance_lifecycle: super::instance_lifecycle::InstanceLifecycleGates,
    ) -> Self {
        Self {
            authority,
            entries: Mutex::new(HashMap::new()),
            lifecycle: Arc::new(AsyncRwLock::new(())),
            instance_lifecycle,
            close_gate: Arc::new(AsyncMutex::new(())),
            phase: AtomicU8::new(ManagedOwnerPhase::Running as u8),
        }
    }

    pub(super) async fn admit(
        &self,
        instance_id: &str,
        instance_lifecycle: OwnedMutexGuard<()>,
        recovery_allowed: bool,
    ) -> Result<AppManagedCompositionAdmission, ManagedCompositionAdmissionError> {
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let lifecycle = self.lifecycle.clone().read_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let entry = self.entry(instance_id)?;
        let gate = entry.gate.clone().lock_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let admission = ManagedCompositionAdmission {
            authority: self.authority.clone(),
            entry: entry.clone(),
            _lifecycle: lifecycle,
            _gate: gate,
        };
        match entry.phase() {
            ManagedEntryPhase::Open => Ok(AppManagedCompositionAdmission::bind(
                admission,
                instance_lifecycle,
            )),
            ManagedEntryPhase::Retired => Err(ManagedCompositionAdmissionError::Retired),
            ManagedEntryPhase::Latched if !recovery_allowed => {
                Err(ManagedCompositionAdmissionError::RecoveryBlockedByActiveSession)
            }
            ManagedEntryPhase::Latched => {
                recover_admission_owned(admission, instance_lifecycle).await
            }
        }
    }

    pub(super) async fn retire(
        &self,
        instance_id: &str,
    ) -> Result<ManagedCompositionRetirement, ManagedCompositionAdmissionError> {
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        let lifecycle = self.lifecycle.clone().read_owned().await;
        let entry = self.entry(instance_id)?;
        let gate = entry.gate.clone().lock_owned().await;
        if self.phase() != ManagedOwnerPhase::Running {
            return Err(ManagedCompositionAdmissionError::Closed);
        }
        match entry.phase() {
            ManagedEntryPhase::Open => entry.store_phase(ManagedEntryPhase::Retired),
            ManagedEntryPhase::Latched => {
                return Err(ManagedCompositionAdmissionError::RecoveryFailed);
            }
            ManagedEntryPhase::Retired => {
                return Err(ManagedCompositionAdmissionError::Retired);
            }
        }
        Ok(ManagedCompositionRetirement {
            entry,
            _lifecycle: lifecycle,
            _gate: gate,
            committed: false,
        })
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
            .cloned()
            .collect::<Vec<_>>();
        entries.sort_by(|left, right| {
            left.identity
                .instance_id()
                .cmp(right.identity.instance_id())
        });
        drop(drained);

        let mut recovery_failed = false;
        for entry in entries {
            if entry.phase() != ManagedEntryPhase::Latched {
                continue;
            }
            let instance_lifecycle = self
                .instance_lifecycle
                .acquire(entry.identity.instance_id())
                .await;
            let gate = entry.gate.clone().lock_owned().await;
            if entry.phase() != ManagedEntryPhase::Latched {
                continue;
            }
            if !recover_entry_owned(self.authority.clone(), entry, instance_lifecycle, gate).await {
                recovery_failed = true;
            }
        }
        if recovery_failed {
            Err(ManagedCompositionCloseError)
        } else {
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
        if let Some(entry) = entries.get(instance_id) {
            return Ok(entry.clone());
        }
        let identity = self.authority.identify(instance_id)?;
        let entry = Arc::new(ManagedInstanceEntry {
            identity,
            gate: Arc::new(AsyncMutex::new(())),
            phase: AtomicU8::new(ManagedEntryPhase::Open as u8),
        });
        entries.insert(instance_id.to_string(), entry.clone());
        Ok(entry)
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
    admission: ManagedCompositionAdmission,
    instance_lifecycle: OwnedMutexGuard<()>,
) -> Result<AppManagedCompositionAdmission, ManagedCompositionAdmissionError> {
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let recovered = admission
            .authority
            .recover_and_inspect(&admission.entry.identity)
            .await
            .is_ok();
        if recovered {
            admission.entry.store_phase(ManagedEntryPhase::Open);
            let _ = completed_tx.send(Ok(AppManagedCompositionAdmission::bind(
                admission,
                instance_lifecycle,
            )));
        } else {
            let _ = completed_tx.send(Err(ManagedCompositionAdmissionError::RecoveryFailed));
        }
    });
    completed_rx
        .await
        .unwrap_or(Err(ManagedCompositionAdmissionError::RecoveryFailed))
}

async fn recover_entry_owned(
    authority: ManagedCompositionAuthority,
    entry: Arc<ManagedInstanceEntry>,
    instance_lifecycle: OwnedMutexGuard<()>,
    gate: OwnedMutexGuard<()>,
) -> bool {
    let (completed_tx, completed_rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        let _instance_lifecycle = instance_lifecycle;
        let _gate = gate;
        let recovered = authority.recover_and_inspect(&entry.identity).await.is_ok();
        if recovered {
            entry.store_phase(ManagedEntryPhase::Open);
        }
        let _ = completed_tx.send(recovered);
    });
    completed_rx.await.unwrap_or(false)
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
            self.entry.store_phase(ManagedEntryPhase::Latched);
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
    use super::{
        ManagedCompositionAdmissionError, ManagedCompositionOwner, ManagedEntryPhase,
        ManagedMutationError,
    };
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
        instance_lifecycle: super::super::instance_lifecycle::InstanceLifecycleGates,
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
            let instance_lifecycle =
                super::super::instance_lifecycle::InstanceLifecycleGates::default();
            Self {
                root,
                owner: Arc::new(ManagedCompositionOwner::claim(
                    authority,
                    instance_lifecycle.clone(),
                )),
                instance_lifecycle,
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
            let lifecycle = self.instance_lifecycle.acquire(instance_id).await;
            self.owner
                .admit(instance_id, lifecycle, recovery_allowed)
                .await
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
    async fn retirement_rolls_back_uncommitted_and_preserves_committed_tombstone() {
        let uncommitted = OwnerFixture::new("retirement-rollback");
        let retirement = uncommitted
            .owner
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
            .owner
            .retire(INSTANCE_A)
            .await
            .expect("retire instance")
            .commit();
        assert!(matches!(
            committed.admit(INSTANCE_A).await,
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
