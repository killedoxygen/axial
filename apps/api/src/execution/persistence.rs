//! Coordinated asynchronous persistence for State-owned snapshots.
//!
//! State decides what is persisted and whether a caller may accept debounce. This
//! module owns process-local capability coordination, bounded scheduling,
//! blocking serialization, and exact-file replacement. The retained application
//! root session and directory capabilities provide the security boundary.

use super::anchored_record::{AnchoredRecordDirectory, AnchoredRecordTarget};
use axial_fs::{
    DirectoryIdentity, EffectOwner, LeafName, LeafNameEquivalenceKey,
    leaf_name_equivalence_keys, leaf_names_equivalent,
};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::{Builder, Handle};
use tokio::sync::{Mutex as AsyncMutex, Notify, oneshot, watch};
use tokio::time::Instant;

const DEFAULT_QUIET_WINDOW: Duration = Duration::from_millis(20);
const DEFAULT_HARD_DEADLINE: Duration = Duration::from_millis(100);
type SnapshotEncoder = Box<dyn FnOnce() -> io::Result<Vec<u8>> + Send + 'static>;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) struct PersistenceRevision(u64);

impl PersistenceRevision {
    pub(crate) const fn get(self) -> u64 {
        self.0
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum WriteUrgency {
    Debounced,
    Immediate,
}

#[derive(Clone, Debug, thiserror::Error)]
pub(crate) enum PersistenceError {
    #[error("persistence owner is already active for this capability scope")]
    DuplicateOwner,
    #[error("persistence target is outside its owner capability scope")]
    TargetOutsideOwner,
    #[error("persistence record was opened with a different target")]
    TargetMismatch,
    #[error("persistence owner is not open")]
    Closed,
    #[error("persistence revision counter overflowed")]
    RevisionOverflow,
    #[error("snapshot serialization failed: {message}")]
    Serialization {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("atomic snapshot write failed: {message}")]
    Write {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("persistence blocking task failed: {message}")]
    BlockingTask { message: String },
    #[error("failed persistence revision has no retryable bytes")]
    RetryUnavailable,
    #[error("persistence worker stopped before resolving the revision")]
    WorkerStopped,
}

impl PersistenceError {
    pub(crate) fn io_kind(&self) -> io::ErrorKind {
        match self {
            Self::Serialization { kind, .. } | Self::Write { kind, .. } => *kind,
            Self::DuplicateOwner | Self::TargetMismatch | Self::Closed => {
                io::ErrorKind::AlreadyExists
            }
            Self::TargetOutsideOwner => io::ErrorKind::PermissionDenied,
            Self::RevisionOverflow
            | Self::BlockingTask { .. }
            | Self::RetryUnavailable
            | Self::WorkerStopped => io::ErrorKind::Other,
        }
    }
}

impl From<PersistenceError> for io::Error {
    fn from(error: PersistenceError) -> Self {
        io::Error::new(error.io_kind(), error)
    }
}

pub(crate) trait AtomicWriteBackend: Send + Sync + 'static {
    fn write(
        &self,
        destination: &AnchoredRecordTarget,
        effects: &EffectOwner,
        contents: &[u8],
    ) -> io::Result<()>;
}

struct FileAtomicWriteBackend;

impl AtomicWriteBackend for FileAtomicWriteBackend {
    fn write(
        &self,
        destination: &AnchoredRecordTarget,
        effects: &EffectOwner,
        contents: &[u8],
    ) -> io::Result<()> {
        destination.write(effects, contents)
    }
}

#[derive(Clone, Copy)]
struct PersistenceSchedule {
    quiet_window: Duration,
    hard_deadline: Duration,
}

#[derive(Clone)]
struct CoordinatorExecutor {
    inner: Arc<CoordinatorExecutorInner>,
}

struct CoordinatorExecutorInner {
    handle: Handle,
    _thread: Option<JoinHandle<()>>,
}

impl CoordinatorExecutor {
    fn process_lifetime() -> Self {
        let (handle_tx, handle_rx) = mpsc::sync_channel(1);
        let thread = std::thread::Builder::new()
            .name("axial-persistence".to_string())
            .spawn(move || {
                let runtime = Builder::new_current_thread()
                    .enable_time()
                    .build()
                    .expect("build process-lifetime persistence runtime");
                handle_tx
                    .send(runtime.handle().clone())
                    .expect("publish process-lifetime persistence handle");
                runtime.block_on(std::future::pending::<()>());
            })
            .expect("spawn process-lifetime persistence thread");
        let handle = handle_rx
            .recv()
            .expect("receive process-lifetime persistence handle");
        Self {
            inner: Arc::new(CoordinatorExecutorInner {
                handle,
                _thread: Some(thread),
            }),
        }
    }

    #[cfg(test)]
    fn captured(handle: Handle) -> Self {
        Self {
            inner: Arc::new(CoordinatorExecutorInner {
                handle,
                _thread: None,
            }),
        }
    }

    fn spawn(&self, future: impl Future<Output = ()> + Send + 'static) {
        drop(self.inner.handle.spawn(future));
    }
}

impl Default for PersistenceSchedule {
    fn default() -> Self {
        Self {
            quiet_window: DEFAULT_QUIET_WINDOW,
            hard_deadline: DEFAULT_HARD_DEADLINE,
        }
    }
}

#[derive(Clone)]
pub(crate) struct PersistenceCoordinator {
    inner: Arc<CoordinatorInner>,
}

struct CoordinatorInner {
    owners: Mutex<HashMap<DirectoryIdentity, DirectoryOwnerClaims>>,
    backend: Arc<dyn AtomicWriteBackend>,
    schedule: PersistenceSchedule,
    executor: CoordinatorExecutor,
}

impl PersistenceCoordinator {
    pub(crate) fn global() -> Self {
        static COORDINATOR: OnceLock<PersistenceCoordinator> = OnceLock::new();
        COORDINATOR
            .get_or_init(|| {
                Self::new(
                    Arc::new(FileAtomicWriteBackend),
                    PersistenceSchedule::default(),
                    CoordinatorExecutor::process_lifetime(),
                )
            })
            .clone()
    }

    fn new(
        backend: Arc<dyn AtomicWriteBackend>,
        schedule: PersistenceSchedule,
        executor: CoordinatorExecutor,
    ) -> Self {
        Self {
            inner: Arc::new(CoordinatorInner {
                owners: Mutex::new(HashMap::new()),
                backend,
                schedule,
                executor,
            }),
        }
    }

    pub(crate) fn claim_directory(
        &self,
        directory: AnchoredRecordDirectory,
    ) -> Result<PersistenceOwnerLease, PersistenceError> {
        self.claim(directory, OwnerScope::Directory)
    }

    pub(crate) fn claim_record(
        &self,
        target: AnchoredRecordTarget,
    ) -> Result<PersistenceOwnerLease, PersistenceError> {
        self.claim(target.directory(), OwnerScope::Record(target))
    }

    fn claim(
        &self,
        directory: AnchoredRecordDirectory,
        scope: OwnerScope,
    ) -> Result<PersistenceOwnerLease, PersistenceError> {
        let directory_identity = directory.identity().map_err(write_error)?;
        let mut owners = self
            .inner
            .owners
            .lock()
            .expect("persistence owner registry lock poisoned");
        owners.retain(|_, claims| claims.retain_live());
        let claims = owners.entry(directory_identity).or_default();
        if claims.conflicts(&scope) {
            return Err(PersistenceError::DuplicateOwner);
        }
        let owner = Arc::new(OwnerInner {
            directory,
            directory_identity,
            scope: scope.clone(),
            effect_transition: Arc::new(AsyncMutex::new(())),
            coordinator: self.inner.clone(),
            state: Mutex::new(OwnerState::default()),
        });
        claims.insert(scope, Arc::downgrade(&owner));
        Ok(PersistenceOwnerLease { inner: owner })
    }

    #[cfg(test)]
    pub(crate) fn for_test(
        backend: Arc<dyn AtomicWriteBackend>,
        quiet_window: Duration,
        hard_deadline: Duration,
    ) -> Self {
        Self::new(
            backend,
            PersistenceSchedule {
                quiet_window,
                hard_deadline,
            },
            CoordinatorExecutor::captured(Handle::current()),
        )
    }
}

#[derive(Clone)]
pub(crate) struct PersistenceOwnerLease {
    inner: Arc<OwnerInner>,
}

struct OwnerInner {
    directory: AnchoredRecordDirectory,
    directory_identity: DirectoryIdentity,
    scope: OwnerScope,
    effect_transition: Arc<AsyncMutex<()>>,
    coordinator: Arc<CoordinatorInner>,
    state: Mutex<OwnerState>,
}

#[derive(Clone)]
enum OwnerScope {
    Directory,
    Record(AnchoredRecordTarget),
}

#[derive(Default)]
struct DirectoryOwnerClaims {
    directory: Option<Weak<OwnerInner>>,
    records: Vec<(LeafName, Weak<OwnerInner>)>,
}

impl DirectoryOwnerClaims {
    fn retain_live(&mut self) -> bool {
        if self
            .directory
            .as_ref()
            .is_some_and(|owner| owner.strong_count() == 0)
        {
            self.directory = None;
        }
        self.records.retain(|(_, owner)| owner.strong_count() > 0);
        self.directory.is_some() || !self.records.is_empty()
    }

    fn conflicts(&self, scope: &OwnerScope) -> bool {
        match scope {
            OwnerScope::Directory => {
                self.directory.as_ref().and_then(Weak::upgrade).is_some()
                    || self.records.iter().any(|(_, owner)| owner.upgrade().is_some())
            }
            OwnerScope::Record(target) => {
                self.directory.as_ref().and_then(Weak::upgrade).is_some()
                    || self.records.iter().any(|(existing, owner)| {
                        owner.upgrade().is_some()
                            && leaf_names_equivalent(
                                existing.as_os_str(),
                                target.leaf().as_os_str(),
                            )
                    })
            }
        }
    }

    fn insert(&mut self, scope: OwnerScope, owner: Weak<OwnerInner>) {
        match scope {
            OwnerScope::Directory => self.directory = Some(owner),
            OwnerScope::Record(target) => self.records.push((target.leaf().clone(), owner)),
        }
    }
}

#[derive(Default)]
struct OwnerState {
    lanes: LaneRegistry,
    lifecycle: OwnerLifecycle,
}

#[derive(Default)]
struct LaneRegistry {
    next_id: u64,
    lanes: HashMap<u64, LaneRegistration>,
    index: HashMap<LeafNameEquivalenceKey, Vec<u64>>,
}

struct LaneRegistration {
    keys: Vec<LeafNameEquivalenceKey>,
    lane: Arc<PathLane>,
}

impl LaneRegistry {
    fn lookup(
        &self,
        destination: &AnchoredRecordTarget,
    ) -> Result<Option<Arc<PathLane>>, PersistenceError> {
        let candidate_ids = leaf_name_equivalence_keys(destination.leaf().as_os_str())
            .iter()
            .filter_map(|key| self.index.get(key))
            .flatten()
            .copied();
        for id in candidate_ids {
            let Some(registration) = self.lanes.get(&id) else {
                continue;
            };
            let lane = &registration.lane;
            if !leaf_names_equivalent(
                lane.destination.leaf().as_os_str(),
                destination.leaf().as_os_str(),
            ) {
                continue;
            }
            if lane.destination.leaf() != destination.leaf()
                || lane.destination.max_existing_bytes() != destination.max_existing_bytes()
            {
                return Err(PersistenceError::TargetMismatch);
            }
            return Ok(Some(lane.clone()));
        }
        Ok(None)
    }

    fn insert(&mut self, lane: Arc<PathLane>) -> Result<(), PersistenceError> {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .ok_or(PersistenceError::RevisionOverflow)?;
        let keys = leaf_name_equivalence_keys(lane.destination.leaf().as_os_str());
        for key in &keys {
            self.index.entry(key.clone()).or_default().push(id);
        }
        self.lanes.insert(id, LaneRegistration { keys, lane });
        Ok(())
    }

    fn remove(&mut self, lane: &Arc<PathLane>) {
        let id = leaf_name_equivalence_keys(lane.destination.leaf().as_os_str())
            .iter()
            .filter_map(|key| self.index.get(key))
            .flatten()
            .find_map(|id| {
                self.lanes
                    .get(id)
                    .is_some_and(|registration| Arc::ptr_eq(&registration.lane, lane))
                    .then_some(*id)
            });
        let Some(id) = id else {
            return;
        };
        let Some(registration) = self.lanes.remove(&id) else {
            return;
        };
        for key in registration.keys {
            let remove_bucket = if let Some(bucket) = self.index.get_mut(&key) {
                bucket.retain(|candidate| *candidate != id);
                bucket.is_empty()
            } else {
                false
            };
            if remove_bucket {
                self.index.remove(&key);
            }
        }
    }

    fn values(&self) -> Vec<Arc<PathLane>> {
        self.lanes
            .values()
            .map(|registration| registration.lane.clone())
            .collect()
    }

    fn clear(&mut self) {
        self.lanes.clear();
        self.index.clear();
    }
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum OwnerLifecycle {
    #[default]
    Open,
    Closing,
    Closed,
}

struct OwnerCloseTransition {
    owner: Arc<OwnerInner>,
    armed: bool,
}

impl OwnerCloseTransition {
    fn new(owner: Arc<OwnerInner>) -> Self {
        Self { owner, armed: true }
    }

    fn finish(mut self, succeeded: bool) {
        let mut state = self
            .owner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        state.lifecycle = if succeeded {
            OwnerLifecycle::Closed
        } else {
            OwnerLifecycle::Open
        };
        if succeeded {
            state.lanes.clear();
        }
        self.armed = false;
    }
}

impl Drop for OwnerCloseTransition {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .owner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        if state.lifecycle == OwnerLifecycle::Closing {
            state.lifecycle = OwnerLifecycle::Open;
        }
    }
}

impl PersistenceOwnerLease {
    pub(crate) fn writer(
        &self,
        destination: AnchoredRecordTarget,
    ) -> Result<AtomicSnapshotWriter, PersistenceError> {
        let destination_identity = destination.directory_identity().map_err(write_error)?;
        if destination_identity != self.inner.directory_identity {
            return Err(PersistenceError::TargetOutsideOwner);
        }
        let destination = match &self.inner.scope {
            OwnerScope::Directory => destination,
            OwnerScope::Record(owned) => {
                if owned.leaf() != destination.leaf() {
                    return Err(PersistenceError::TargetOutsideOwner);
                }
                if owned.max_existing_bytes() != destination.max_existing_bytes() {
                    return Err(PersistenceError::TargetMismatch);
                }
                owned.clone()
            }
        };

        let mut owner_state = self
            .inner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        if owner_state.lifecycle != OwnerLifecycle::Open {
            return Err(PersistenceError::Closed);
        }
        if let Some(lane) = owner_state.lanes.lookup(&destination)? {
            return Ok(AtomicSnapshotWriter {
                lane,
                owner: self.inner.clone(),
            });
        }

        let (progress, _) = watch::channel(CommitProgress::default());
        let lane = Arc::new(PathLane {
            destination,
            effects: Mutex::new(None),
            backend: self.inner.coordinator.backend.clone(),
            schedule: self.inner.coordinator.schedule,
            state: Mutex::new(LaneState::default()),
            progress,
            changed: Notify::new(),
            idle: Notify::new(),
        });
        owner_state.lanes.insert(lane.clone())?;
        Ok(AtomicSnapshotWriter {
            lane,
            owner: self.inner.clone(),
        })
    }

    pub(crate) async fn flush(&self) -> Result<(), PersistenceError> {
        await_all_lanes(&self.inner, self.live_lanes()).await
    }

    pub(crate) async fn close(&self) -> Result<(), PersistenceError> {
        let lanes = {
            let mut state = self
                .inner
                .state
                .lock()
                .expect("persistence owner state lock poisoned");
            match state.lifecycle {
                OwnerLifecycle::Open => state.lifecycle = OwnerLifecycle::Closing,
                OwnerLifecycle::Closing => return Err(PersistenceError::Closed),
                OwnerLifecycle::Closed => return Ok(()),
            }
            live_owner_lanes(&mut state)
        };
        let transition = OwnerCloseTransition::new(self.inner.clone());
        let mut result = await_all_lanes(&self.inner, lanes.clone()).await;
        if result.is_ok() {
            await_all_lanes_idle(&lanes).await;
            result = await_all_lane_deletions(&lanes).await;
        }
        let succeeded = result.is_ok();
        if succeeded
            && let Err(error) = settle_owner_capabilities(self.inner.clone(), lanes).await
        {
            transition.finish(false);
            return Err(error);
        }
        transition.finish(succeeded);
        if succeeded {
            self.release_closed_registration();
        }
        result
    }

    fn release_closed_registration(&self) {
        let mut owners = self
            .inner
            .coordinator
            .owners
            .lock()
            .expect("persistence owner registry lock poisoned");
        if let Some(claims) = owners.get_mut(&self.inner.directory_identity) {
            match &self.inner.scope {
                OwnerScope::Directory => {
                    if claims
                        .directory
                        .as_ref()
                        .and_then(Weak::upgrade)
                        .is_some_and(|owner| Arc::ptr_eq(&owner, &self.inner))
                    {
                        claims.directory = None;
                    }
                }
                OwnerScope::Record(target) => claims.records.retain(|(claimed, owner)| {
                    !leaf_names_equivalent(claimed.as_os_str(), target.leaf().as_os_str())
                        || owner
                            .upgrade()
                            .is_some_and(|owner| !Arc::ptr_eq(&owner, &self.inner))
                }),
            }
            if !claims.retain_live() {
                owners.remove(&self.inner.directory_identity);
            }
        }
    }

    fn live_lanes(&self) -> Vec<Arc<PathLane>> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        live_owner_lanes(&mut state)
    }
}

fn live_owner_lanes(state: &mut OwnerState) -> Vec<Arc<PathLane>> {
    state.lanes.values()
}

#[derive(Clone)]
pub(crate) struct AtomicSnapshotWriter {
    lane: Arc<PathLane>,
    owner: Arc<OwnerInner>,
}

impl Drop for AtomicSnapshotWriter {
    fn drop(&mut self) {
        let mut owner_state = self
            .owner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        if Arc::strong_count(&self.lane) != 2 {
            return;
        }
        if !lane_is_quiescent(&self.lane) {
            return;
        }
        owner_state.lanes.remove(&self.lane);
    }
}

fn lane_is_quiescent(lane: &PathLane) -> bool {
    let lane_state = lane.state.lock().expect("persistence lane lock poisoned");
    let quiescent = lane_state.lifecycle == RecordLifecycle::Open
        && lane_state.pending.is_none()
        && lane_state.in_flight_revision.is_none()
        && lane_state.failed_retry.is_none()
        && !lane_state.worker_running;
    drop(lane_state);
    quiescent
        && lane
            .effects
            .lock()
            .expect("persistence lane effect owner lock poisoned")
            .is_none()
}

fn prune_completed_lane(owner: &OwnerInner, lane: &Arc<PathLane>, expected_strong_count: usize) {
    let mut owner_state = owner
        .state
        .lock()
        .expect("persistence owner state lock poisoned");
    if Arc::strong_count(lane) == expected_strong_count && lane_is_quiescent(lane) {
        owner_state.lanes.remove(lane);
    }
}

struct PathLane {
    destination: AnchoredRecordTarget,
    effects: Mutex<Option<EffectOwner>>,
    backend: Arc<dyn AtomicWriteBackend>,
    schedule: PersistenceSchedule,
    state: Mutex<LaneState>,
    progress: watch::Sender<CommitProgress>,
    changed: Notify,
    idle: Notify,
}

#[derive(Default)]
struct LaneState {
    lifecycle: RecordLifecycle,
    next_revision: u64,
    committed_revision: u64,
    pending: Option<PendingWrite>,
    pending_immediate: bool,
    quiet_deadline: Option<Instant>,
    hard_deadline: Option<Instant>,
    failed_retry: Option<RetryWrite>,
    in_flight_revision: Option<u64>,
    worker_running: bool,
    delete_failure: Option<PersistenceError>,
    #[cfg(test)]
    injected_worker_panics: usize,
}

#[derive(Clone, Copy, Default, Eq, PartialEq)]
enum RecordLifecycle {
    #[default]
    Open,
    Deleting,
    DeletePending,
    Deleted,
}

struct RecordDeleteTransition {
    lane: Arc<PathLane>,
    previous: RecordLifecycle,
    previous_failure: Option<PersistenceError>,
    armed: bool,
}

impl RecordDeleteTransition {
    fn new(
        lane: Arc<PathLane>,
        previous: RecordLifecycle,
        previous_failure: Option<PersistenceError>,
    ) -> Self {
        Self {
            lane,
            previous,
            previous_failure,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RecordDeleteTransition {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let mut state = self
            .lane
            .state
            .lock()
            .expect("persistence lane lock poisoned");
        if state.lifecycle == RecordLifecycle::Deleting {
            state.lifecycle = self.previous;
            state.delete_failure = self.previous_failure.take();
        }
        self.lane.idle.notify_waiters();
    }
}

struct PendingWrite {
    revision: u64,
    payload: WritePayload,
}

enum WritePayload {
    Encode(SnapshotEncoder),
    Encoded(Vec<u8>),
}

struct RetryWrite {
    revision: u64,
    contents: Vec<u8>,
}

#[derive(Clone, Default)]
struct CommitProgress {
    committed_revision: u64,
    failure: Option<(u64, PersistenceError)>,
}

pub(crate) struct AcceptedWrite {
    revision: PersistenceRevision,
    progress: watch::Receiver<CommitProgress>,
    executor: CoordinatorExecutor,
}

impl AcceptedWrite {
    pub(crate) const fn revision(&self) -> PersistenceRevision {
        self.revision
    }

    pub(crate) async fn persisted(mut self) -> Result<PersistenceRevision, PersistenceError> {
        loop {
            {
                let progress = self.progress.borrow();
                if progress.committed_revision >= self.revision.0 {
                    return Ok(PersistenceRevision(progress.committed_revision));
                }
                if let Some((failed_revision, error)) = &progress.failure
                    && *failed_revision >= self.revision.0
                {
                    return Err(error.clone());
                }
            }
            self.progress
                .changed()
                .await
                .map_err(|_| PersistenceError::WorkerStopped)?;
        }
    }

    pub(crate) fn observe(
        self,
        completed: impl FnOnce(Result<PersistenceRevision, PersistenceError>) + Send + 'static,
    ) {
        let executor = self.executor.clone();
        executor.spawn(async move {
            completed(self.persisted().await);
        });
    }

    pub(crate) fn observe_async<Completed, Completion>(self, completed: Completed)
    where
        Completed:
            FnOnce(Result<PersistenceRevision, PersistenceError>) -> Completion + Send + 'static,
        Completion: Future<Output = ()> + Send + 'static,
    {
        let executor = self.executor.clone();
        executor.spawn(async move {
            completed(self.persisted().await).await;
        });
    }
}

impl AtomicSnapshotWriter {
    pub(crate) async fn delete(&self) -> Result<(), PersistenceError> {
        let (previous, previous_failure) = {
            let owner = self
                .owner
                .state
                .lock()
                .expect("persistence owner state lock poisoned");
            if owner.lifecycle != OwnerLifecycle::Open {
                return Err(PersistenceError::Closed);
            }
            let mut state = self
                .lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            let previous = match state.lifecycle {
                RecordLifecycle::Open | RecordLifecycle::DeletePending => state.lifecycle,
                RecordLifecycle::Deleted => return Ok(()),
                RecordLifecycle::Deleting => {
                    return Err(PersistenceError::Closed);
                }
            };
            state.lifecycle = RecordLifecycle::Deleting;
            let previous_failure = state.delete_failure.take();
            (previous, previous_failure)
        };
        let transition =
            RecordDeleteTransition::new(self.lane.clone(), previous, previous_failure);
        self.flush().await?;
        await_all_lanes_idle(std::slice::from_ref(&self.lane)).await;

        let lane = self.lane.clone();
        let owner = self.owner.clone();
        let executor = owner.coordinator.executor.clone();
        let (completed, completion) = oneshot::channel();
        executor.spawn(async move {
            let mut transition = transition;
            let blocking_lane = lane.clone();
            let effect_transition = owner.effect_transition.clone().lock_owned().await;
            let result = tokio::task::spawn_blocking(move || {
                let _transition = effect_transition;
                with_lane_effect_owner(&blocking_lane, |destination, effects| {
                    destination.remove(effects)
                })
                .map_err(write_error)
            })
            .await
            .unwrap_or_else(|error| {
                Err(PersistenceError::BlockingTask {
                    message: format!("persistence delete task failed: {error}"),
                })
            });
            let mut state = lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            state.lifecycle = if result.is_ok() {
                RecordLifecycle::Deleted
            } else {
                RecordLifecycle::DeletePending
            };
            state.delete_failure = result.as_ref().err().cloned();
            drop(state);
            transition.disarm();
            lane.idle.notify_waiters();
            if result.is_ok() {
                owner
                    .state
                    .lock()
                    .expect("persistence owner state lock poisoned")
                    .lanes
                    .remove(&lane);
            }
            let _ = completed.send(result);
        });
        completion.await.map_err(|_| PersistenceError::WorkerStopped)?
    }

    #[cfg(test)]
    pub(crate) fn exhaust_revisions_for_test(&self) {
        self.lane
            .state
            .lock()
            .expect("persistence lane state lock poisoned")
            .next_revision = u64::MAX;
    }

    pub(crate) fn accept<T, Encode>(
        &self,
        value: T,
        urgency: WriteUrgency,
        encode: Encode,
    ) -> Result<AcceptedWrite, PersistenceError>
    where
        T: Send + 'static,
        Encode: FnOnce(T) -> io::Result<Vec<u8>> + Send + 'static,
    {
        let encoder: SnapshotEncoder = Box::new(move || encode(value));
        self.accept_payload(WritePayload::Encode(encoder), urgency)
    }

    pub(crate) fn accept_encoded(
        &self,
        contents: Vec<u8>,
        urgency: WriteUrgency,
    ) -> Result<AcceptedWrite, PersistenceError> {
        self.accept_payload(WritePayload::Encoded(contents), urgency)
    }

    fn accept_payload(
        &self,
        payload: WritePayload,
        urgency: WriteUrgency,
    ) -> Result<AcceptedWrite, PersistenceError> {
        let (ticket, start_worker) = {
            let owner_state = self
                .owner
                .state
                .lock()
                .expect("persistence owner state lock poisoned");
            if owner_state.lifecycle != OwnerLifecycle::Open {
                return Err(PersistenceError::Closed);
            }
            let mut state = self
                .lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            if state.lifecycle != RecordLifecycle::Open {
                return Err(PersistenceError::Closed);
            }
            let revision = state
                .next_revision
                .checked_add(1)
                .ok_or(PersistenceError::RevisionOverflow)?;
            state.next_revision = revision;
            state.failed_retry = None;
            let now = Instant::now();
            if state.pending.is_none() {
                state.pending_immediate = false;
                state.hard_deadline = Some(now + self.lane.schedule.hard_deadline);
            }
            state.pending = Some(PendingWrite { revision, payload });
            let hard_deadline = state
                .hard_deadline
                .expect("pending persistence has a hard deadline");
            state.pending_immediate |= urgency == WriteUrgency::Immediate;
            state.quiet_deadline = Some(if state.pending_immediate {
                now
            } else {
                std::cmp::min(now + self.lane.schedule.quiet_window, hard_deadline)
            });
            let ticket = self.ticket(revision);
            let start_worker = !state.worker_running;
            state.worker_running = true;
            (ticket, start_worker)
        };

        if start_worker {
            spawn_lane_worker(self.lane.clone(), self.owner.clone());
        } else {
            self.lane.changed.notify_one();
        }
        Ok(ticket)
    }

    #[cfg(test)]
    pub(crate) async fn persist<T, Encode>(
        &self,
        value: T,
        encode: Encode,
    ) -> Result<PersistenceRevision, PersistenceError>
    where
        T: Send + 'static,
        Encode: FnOnce(T) -> io::Result<Vec<u8>> + Send + 'static,
    {
        self.accept(value, WriteUrgency::Immediate, encode)?
            .persisted()
            .await
    }

    #[cfg(test)]
    pub(crate) fn latest_revision(&self) -> PersistenceRevision {
        let state = self
            .lane
            .state
            .lock()
            .expect("persistence lane lock poisoned");
        PersistenceRevision(state.next_revision)
    }

    pub(crate) async fn flush(&self) -> Result<PersistenceRevision, PersistenceError> {
        self.ticket_for_latest()?.persisted().await
    }

    pub(crate) async fn settle(&self) -> Result<PersistenceRevision, PersistenceError> {
        let revision = match self.flush().await {
            Ok(revision) => revision,
            Err(_) => self.retry()?.persisted().await?,
        };
        await_all_lanes_idle(std::slice::from_ref(&self.lane)).await;
        Ok(revision)
    }

    /// Waits for the current worker to stop without flushing, retrying, or closing its owner.
    pub(crate) async fn wait_until_idle(&self) {
        await_all_lanes_idle(std::slice::from_ref(&self.lane)).await;
    }

    pub(crate) fn retry(&self) -> Result<AcceptedWrite, PersistenceError> {
        let (ticket, start_worker) = {
            let owner_state = self
                .owner
                .state
                .lock()
                .expect("persistence owner state lock poisoned");
            if owner_state.lifecycle != OwnerLifecycle::Open {
                return Err(PersistenceError::Closed);
            }
            let mut state = self
                .lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            if state.lifecycle != RecordLifecycle::Open {
                return Err(PersistenceError::Closed);
            }
            let retry = state
                .failed_retry
                .take()
                .ok_or(PersistenceError::RetryUnavailable)?;
            if state.pending.is_some() || retry.revision != state.next_revision {
                return Err(PersistenceError::RetryUnavailable);
            }
            let now = Instant::now();
            state.pending = Some(PendingWrite {
                revision: retry.revision,
                payload: WritePayload::Encoded(retry.contents),
            });
            state.pending_immediate = true;
            state.quiet_deadline = Some(now);
            state.hard_deadline = Some(now);
            self.lane.progress.send_replace(CommitProgress {
                committed_revision: state.committed_revision,
                failure: None,
            });
            let ticket = self.ticket(retry.revision);
            let start_worker = !state.worker_running;
            state.worker_running = true;
            (ticket, start_worker)
        };
        if start_worker {
            spawn_lane_worker(self.lane.clone(), self.owner.clone());
        } else {
            self.lane.changed.notify_one();
        }
        Ok(ticket)
    }

    fn ticket_for_latest(&self) -> Result<AcceptedWrite, PersistenceError> {
        let (revision, start_worker, notify_worker) = {
            let mut state = self
                .lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            if state.pending.is_some() {
                state.pending_immediate = true;
                state.quiet_deadline = Some(Instant::now());
            }
            let has_work = state.pending.is_some() || state.in_flight_revision.is_some();
            let start_worker = has_work && !state.worker_running;
            if start_worker {
                state.worker_running = true;
            }
            (state.next_revision, start_worker, has_work && !start_worker)
        };
        if start_worker {
            spawn_lane_worker(self.lane.clone(), self.owner.clone());
        } else if notify_worker {
            self.lane.changed.notify_one();
        }
        Ok(self.ticket(revision))
    }

    fn ticket(&self, revision: u64) -> AcceptedWrite {
        AcceptedWrite {
            revision: PersistenceRevision(revision),
            progress: self.lane.progress.subscribe(),
            executor: self.owner.coordinator.executor.clone(),
        }
    }

    #[cfg(test)]
    fn queue_shape(&self) -> (usize, usize) {
        let state = self
            .lane
            .state
            .lock()
            .expect("persistence lane lock poisoned");
        (
            usize::from(state.pending.is_some()),
            usize::from(state.in_flight_revision.is_some()),
        )
    }

    #[cfg(test)]
    fn pending_is_immediate(&self) -> bool {
        self.lane
            .state
            .lock()
            .expect("persistence lane lock poisoned")
            .pending_immediate
    }

    #[cfg(test)]
    fn panic_next_worker(&self) {
        self.lane
            .state
            .lock()
            .expect("persistence lane lock poisoned")
            .injected_worker_panics += 1;
    }
}

fn spawn_lane_worker(lane: Arc<PathLane>, owner: Arc<OwnerInner>) {
    let executor = owner.coordinator.executor.clone();
    executor.spawn(run_lane(lane, owner));
}

fn restart_lane_worker_if_needed(lane: Arc<PathLane>, owner: Arc<OwnerInner>) {
    let start = {
        let mut state = lane.state.lock().expect("persistence lane lock poisoned");
        let has_work = state.pending.is_some() || state.in_flight_revision.is_some();
        if has_work && !state.worker_running {
            state.worker_running = true;
            true
        } else {
            false
        }
    };
    if start {
        spawn_lane_worker(lane, owner);
    }
}

struct LaneWorkerGuard {
    lane: Arc<PathLane>,
    owner: Arc<OwnerInner>,
    armed: bool,
}

impl LaneWorkerGuard {
    fn new(lane: Arc<PathLane>, owner: Arc<OwnerInner>) -> Self {
        Self { lane, owner, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for LaneWorkerGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let restart = {
            let mut state = self
                .lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            state.worker_running = false;
            let restart = state.pending.is_some() || state.in_flight_revision.is_some();
            if restart {
                state.worker_running = true;
            }
            restart
        };
        if restart {
            spawn_lane_worker(self.lane.clone(), self.owner.clone());
        } else {
            prune_completed_lane(&self.owner, &self.lane, 3);
        }
        self.lane.idle.notify_one();
    }
}

async fn run_lane(lane: Arc<PathLane>, owner: Arc<OwnerInner>) {
    let mut guard = LaneWorkerGuard::new(lane.clone(), owner.clone());
    #[cfg(test)]
    {
        let panic_now = {
            let mut state = lane.state.lock().expect("persistence lane lock poisoned");
            if state.injected_worker_panics > 0 {
                state.injected_worker_panics -= 1;
                true
            } else {
                false
            }
        };
        assert!(!panic_now, "injected persistence worker panic");
    }
    loop {
        let deadline = {
            let mut state = lane.state.lock().expect("persistence lane lock poisoned");
            if state.in_flight_revision.is_some() {
                None
            } else if state.pending.is_none() {
                state.worker_running = false;
                guard.disarm();
                drop(state);
                drop(guard);
                prune_completed_lane(&owner, &lane, 2);
                lane.idle.notify_one();
                return;
            } else {
                state.quiet_deadline
            }
        };

        let Some(deadline) = deadline else {
            lane.changed.notified().await;
            continue;
        };
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {}
            () = lane.changed.notified() => continue,
        }

        let pending = {
            let mut state = lane.state.lock().expect("persistence lane lock poisoned");
            if state.in_flight_revision.is_some()
                || state
                    .quiet_deadline
                    .is_some_and(|deadline| Instant::now() < deadline)
            {
                continue;
            }
            let Some(pending) = state.pending.take() else {
                continue;
            };
            state.in_flight_revision = Some(pending.revision);
            state.pending_immediate = false;
            state.quiet_deadline = None;
            state.hard_deadline = None;
            pending
        };

        let physical_lane = lane.clone();
        let physical_owner = owner.clone();
        let effect_transition = owner.effect_transition.clone().lock_owned().await;
        drop(tokio::task::spawn_blocking(move || {
            let _transition = effect_transition;
            let revision = pending.revision;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_blocking_write(
                    &physical_lane,
                    pending.payload,
                )
            }))
            .unwrap_or_else(|panic| {
                BlockingWriteOutcome::SerializationFailed(PersistenceError::BlockingTask {
                    message: panic_payload_message(panic),
                })
            });
            complete_blocking_write(physical_lane, physical_owner, revision, outcome);
        }));
    }
}

fn panic_payload_message(panic: Box<dyn std::any::Any + Send>) -> String {
    if let Some(message) = panic.downcast_ref::<&str>() {
        (*message).to_string()
    } else if let Some(message) = panic.downcast_ref::<String>() {
        message.clone()
    } else {
        "persistence blocking task panicked".to_string()
    }
}

fn complete_blocking_write(
    lane: Arc<PathLane>,
    owner: Arc<OwnerInner>,
    revision: u64,
    outcome: BlockingWriteOutcome,
) {
    {
        let mut state = lane.state.lock().expect("persistence lane lock poisoned");
        if state.in_flight_revision != Some(revision) {
            return;
        }
        state.in_flight_revision = None;
        match outcome {
            BlockingWriteOutcome::Written => {
                state.committed_revision = state.committed_revision.max(revision);
                state.failed_retry = None;
                lane.progress.send_replace(CommitProgress {
                    committed_revision: state.committed_revision,
                    failure: None,
                });
            }
            BlockingWriteOutcome::SerializationFailed(error) => {
                state.failed_retry = None;
                publish_failure_if_latest(&lane, &state, revision, error);
            }
            BlockingWriteOutcome::WriteFailed(error, contents) => {
                if state
                    .pending
                    .as_ref()
                    .is_none_or(|pending| pending.revision <= revision)
                {
                    state.failed_retry = Some(RetryWrite { revision, contents });
                }
                publish_failure_if_latest(&lane, &state, revision, error);
            }
        }
    }
    lane.changed.notify_one();
    restart_lane_worker_if_needed(lane, owner);
}

enum BlockingWriteOutcome {
    Written,
    SerializationFailed(PersistenceError),
    WriteFailed(PersistenceError, Vec<u8>),
}

fn run_blocking_write(
    lane: &PathLane,
    payload: WritePayload,
) -> BlockingWriteOutcome {
    let contents = match payload {
        WritePayload::Encode(encode) => match encode() {
            Ok(contents) => contents,
            Err(error) => {
                return BlockingWriteOutcome::SerializationFailed(
                    PersistenceError::Serialization {
                        kind: error.kind(),
                        message: error.to_string(),
                    },
                );
            }
        },
        WritePayload::Encoded(contents) => contents,
    };
    match with_lane_effect_owner(lane, |destination, effects| {
        lane.backend.write(destination, effects, &contents)
    }) {
        Ok(()) => BlockingWriteOutcome::Written,
        Err(error) => BlockingWriteOutcome::WriteFailed(
            PersistenceError::Write {
                kind: error.kind(),
                message: error.to_string(),
            },
            contents,
        ),
    }
}

fn publish_failure_if_latest(
    lane: &PathLane,
    state: &LaneState,
    revision: u64,
    error: PersistenceError,
) {
    if state
        .pending
        .as_ref()
        .is_some_and(|pending| pending.revision > revision)
    {
        return;
    }
    lane.progress.send_replace(CommitProgress {
        committed_revision: state.committed_revision,
        failure: Some((revision, error)),
    });
}

async fn await_all_lanes(
    owner: &Arc<OwnerInner>,
    lanes: Vec<Arc<PathLane>>,
) -> Result<(), PersistenceError> {
    let mut first_error = None;
    for lane in lanes {
        if let Err(error) = (AtomicSnapshotWriter {
            lane,
            owner: owner.clone(),
        })
        .flush()
        .await
            && first_error.is_none()
        {
            first_error = Some(error);
        }
    }
    first_error.map_or(Ok(()), Err)
}

async fn await_all_lanes_idle(lanes: &[Arc<PathLane>]) {
    for lane in lanes {
        loop {
            let idle = lane.idle.notified();
            if !lane
                .state
                .lock()
                .expect("persistence lane lock poisoned")
                .worker_running
            {
                break;
            }
            idle.await;
        }
    }
}

async fn await_all_lane_deletions(lanes: &[Arc<PathLane>]) -> Result<(), PersistenceError> {
    for lane in lanes {
        loop {
            let completed = lane.idle.notified();
            let state = lane
                .state
                .lock()
                .expect("persistence lane lock poisoned");
            let lifecycle = state.lifecycle;
            match lifecycle {
                RecordLifecycle::Deleting => {
                    drop(state);
                    completed.await;
                }
                RecordLifecycle::DeletePending => {
                    return Err(state.delete_failure.clone().unwrap_or_else(|| {
                        PersistenceError::Write {
                            kind: io::ErrorKind::Other,
                            message: "persistence record deletion remains pending".to_string(),
                        }
                    }));
                }
                RecordLifecycle::Open | RecordLifecycle::Deleted => break,
            }
        }
    }
    Ok(())
}

fn with_lane_effect_owner<T>(
    lane: &PathLane,
    operation: impl FnOnce(&AnchoredRecordTarget, &EffectOwner) -> io::Result<T>,
) -> io::Result<T> {
    let mut retained = lane
        .effects
        .lock()
        .expect("persistence lane effect owner lock poisoned");
    let effects = match retained.take() {
        Some(effects) => effects,
        None => lane.destination.directory().effect_owner()?,
    };
    let outcome = operation(&lane.destination, &effects);
    let target_settlement = lane.destination.settle(&effects);
    let effect_settlement = settle_effect_owner(&effects);
    if let Err(error) = effect_settlement {
        *retained = Some(effects);
        return match outcome {
            Ok(_) => Err(error),
            Err(operation_error) => Err(io::Error::new(
                operation_error.kind(),
                format!("{operation_error}; effect settlement failed: {error}"),
            )),
        };
    }
    match outcome {
        Err(error) => Err(error),
        Ok(value) => target_settlement.map(|()| value),
    }
}

fn settle_effect_owner(effects: &EffectOwner) -> io::Result<()> {
    effects.settle()?;
    effects.require_settled()
}

async fn settle_owner_capabilities(
    owner: Arc<OwnerInner>,
    lanes: Vec<Arc<PathLane>>,
) -> Result<(), PersistenceError> {
    let executor = owner.coordinator.executor.clone();
    let (completed, completion) = oneshot::channel();
    executor.spawn(async move {
        let effect_transition = owner.effect_transition.clone().lock_owned().await;
        let result = tokio::task::spawn_blocking(move || {
            let _transition = effect_transition;
            for lane in lanes {
                let mut retained = lane
                    .effects
                    .lock()
                    .expect("persistence lane effect owner lock poisoned");
                let Some(effects) = retained.take() else {
                    continue;
                };
                let target_settlement = lane.destination.settle(&effects);
                if let Err(error) = settle_effect_owner(&effects) {
                    *retained = Some(effects);
                    return Err(error);
                }
                target_settlement?;
            }
            Ok(())
        })
        .await
        .unwrap_or_else(|error| {
            Err(io::Error::other(format!(
                "persistence effect settlement task failed: {error}"
            )))
        })
        .map_err(write_error);
        let _ = completed.send(result);
    });
    completion.await.map_err(|_| PersistenceError::WorkerStopped)?
}

fn write_error(error: io::Error) -> PersistenceError {
    PersistenceError::Write {
        kind: error.kind(),
        message: error.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};
    use std::sync::Condvar;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
    use std::thread::ThreadId;

    struct RecordingBackend {
        writes: Mutex<Vec<(PathBuf, Vec<u8>)>>,
        failures: AtomicUsize,
        active: AtomicUsize,
        max_active: AtomicUsize,
        delay: Duration,
        threads: Mutex<Vec<ThreadId>>,
        started: Notify,
        gate: Mutex<Option<Arc<PhysicalWriteGate>>>,
    }

    struct PhysicalWriteGate {
        released: Mutex<bool>,
        changed: Condvar,
    }

    impl PhysicalWriteGate {
        fn release(&self) {
            *self.released.lock().expect("physical gate lock") = true;
            self.changed.notify_all();
        }

        fn wait(&self) {
            let mut released = self.released.lock().expect("physical gate lock");
            while !*released {
                released = self.changed.wait(released).expect("physical gate wait");
            }
        }
    }

    impl RecordingBackend {
        fn new(delay: Duration) -> Self {
            Self {
                writes: Mutex::new(Vec::new()),
                failures: AtomicUsize::new(0),
                active: AtomicUsize::new(0),
                max_active: AtomicUsize::new(0),
                delay,
                threads: Mutex::new(Vec::new()),
                started: Notify::new(),
                gate: Mutex::new(None),
            }
        }

        fn gate_next(&self) -> Arc<PhysicalWriteGate> {
            let gate = Arc::new(PhysicalWriteGate {
                released: Mutex::new(false),
                changed: Condvar::new(),
            });
            *self.gate.lock().expect("recording backend gate lock") = Some(gate.clone());
            gate
        }

        fn fail_next(&self) {
            self.failures.fetch_add(1, Ordering::Relaxed);
        }

        fn write_count(&self) -> usize {
            self.writes.lock().expect("recording backend lock").len()
        }

        fn latest_contents(&self) -> Vec<u8> {
            self.writes
                .lock()
                .expect("recording backend lock")
                .last()
                .expect("recorded write")
                .1
                .clone()
        }
    }

    impl AtomicWriteBackend for RecordingBackend {
        fn write(
            &self,
            destination: &AnchoredRecordTarget,
            _effects: &EffectOwner,
            contents: &[u8],
        ) -> io::Result<()> {
            self.threads
                .lock()
                .expect("recording backend thread lock")
                .push(std::thread::current().id());
            let active = self.active.fetch_add(1, Ordering::SeqCst) + 1;
            self.max_active.fetch_max(active, Ordering::SeqCst);
            self.started.notify_one();
            if let Some(gate) = self
                .gate
                .lock()
                .expect("recording backend gate lock")
                .take()
            {
                gate.wait();
            }
            if !self.delay.is_zero() {
                std::thread::sleep(self.delay);
            }
            let should_fail = self
                .failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |failures| {
                    (failures > 0).then(|| failures - 1)
                })
                .is_ok();
            if should_fail {
                self.active.fetch_sub(1, Ordering::SeqCst);
                return Err(io::Error::other("injected atomic write failure"));
            }
            self.writes
                .lock()
                .expect("recording backend lock")
                .push((destination.test_path(), contents.to_vec()));
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn unique_root(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join("persistence-tests")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
    }

    fn test_directory(path: &Path) -> AnchoredRecordDirectory {
        std::fs::create_dir_all(path).expect("create persistence test directory");
        AnchoredRecordDirectory::for_test_directory(path).expect("open persistence test directory")
    }

    fn test_writer(
        owner: &PersistenceOwnerLease,
        destination: &Path,
    ) -> Result<AtomicSnapshotWriter, PersistenceError> {
        let leaf = destination.file_name().expect("test destination leaf");
        let record = owner
            .inner
            .directory
            .target(leaf, 1024 * 1024)
            .expect("test record target");
        owner.writer(record)
    }

    fn fixture(
        name: &str,
        delay: Duration,
        quiet: Duration,
        hard: Duration,
    ) -> (
        Arc<RecordingBackend>,
        PersistenceOwnerLease,
        AtomicSnapshotWriter,
    ) {
        let backend = Arc::new(RecordingBackend::new(delay));
        let coordinator = PersistenceCoordinator::for_test(backend.clone(), quiet, hard);
        let root = unique_root(name);
        let owner = coordinator
            .claim_directory(test_directory(&root))
            .expect("claim owner");
        let writer = test_writer(&owner, &root.join("snapshot.json"))
            .expect("create writer");
        (backend, owner, writer)
    }

    fn encode_number(value: usize) -> io::Result<Vec<u8>> {
        Ok(value.to_string().into_bytes())
    }

    #[test]
    fn process_executor_survives_the_accepting_runtime_shutdown() {
        let root = unique_root("process-executor");
        let destination = root.join("snapshot.json");
        let owner = PersistenceCoordinator::global()
            .claim_directory(test_directory(&root))
            .expect("claim process owner");
        let writer = test_writer(&owner, &destination)
            .expect("process writer");
        let accepting_runtime = Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("accepting runtime");
        let ticket = accepting_runtime.block_on(async {
            writer
                .accept(13, WriteUrgency::Debounced, encode_number)
                .expect("accept process snapshot")
        });
        drop(accepting_runtime);

        Builder::new_current_thread()
            .enable_time()
            .build()
            .expect("waiting runtime")
            .block_on(ticket.persisted())
            .expect("process executor persisted");
        assert_eq!(std::fs::read(&destination).expect("read snapshot"), b"13");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn burst_coalesces_and_persists_the_latest_revision() {
        let (backend, _owner, writer) = fixture(
            "coalesces",
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let mut latest = None;
        for value in 0..200 {
            latest = Some(
                writer
                    .accept(value, WriteUrgency::Debounced, encode_number)
                    .expect("accept snapshot"),
            );
        }

        let latest = latest.expect("latest ticket");
        assert_eq!(latest.revision(), writer.latest_revision());
        let committed = latest.persisted().await.expect("latest persisted");

        assert_eq!(committed, writer.latest_revision());
        assert_eq!(backend.latest_contents(), b"199");
        assert!(backend.write_count() < 10);
    }

    #[tokio::test]
    async fn same_path_handles_serialize_physical_writes() {
        let (backend, owner, first) = fixture(
            "serialized",
            Duration::from_millis(10),
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        let second = owner
            .writer(first.lane.destination.clone())
            .expect("second writer handle");
        let mut latest = None;
        for value in 0..100 {
            let writer = if value % 2 == 0 { &first } else { &second };
            latest = Some(
                writer
                    .accept(value, WriteUrgency::Immediate, encode_number)
                    .expect("accept snapshot"),
            );
        }
        latest
            .expect("latest ticket")
            .persisted()
            .await
            .expect("latest persisted");

        assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
        assert_eq!(backend.latest_contents(), b"99");
    }

    #[tokio::test]
    async fn cancelling_a_waiter_does_not_cancel_the_accepted_write() {
        let (backend, _owner, writer) = fixture(
            "cancelled-waiter",
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let ticket = writer
            .accept(7, WriteUrgency::Debounced, encode_number)
            .expect("accept snapshot");
        let waiter = tokio::spawn(ticket.persisted());
        waiter.abort();
        assert!(waiter.await.expect_err("cancel waiter").is_cancelled());

        writer.flush().await.expect("flush accepted write");
        assert_eq!(backend.latest_contents(), b"7");
    }

    #[tokio::test]
    async fn acceptance_from_a_standard_thread_uses_the_captured_executor() {
        let (backend, _owner, writer) = fixture(
            "standard-thread",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let ticket = std::thread::spawn(move || {
            writer
                .accept(11, WriteUrgency::Immediate, encode_number)
                .expect("accept off runtime")
        })
        .join()
        .expect("standard acceptance thread");

        ticket
            .persisted()
            .await
            .expect("off-runtime write persisted");
        assert_eq!(backend.latest_contents(), b"11");
    }

    #[tokio::test]
    async fn worker_panic_guard_restarts_pending_work() {
        let (backend, _owner, writer) = fixture(
            "worker-panic",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        writer.panic_next_worker();
        writer
            .persist(14, encode_number)
            .await
            .expect("restarted worker persisted");

        assert_eq!(backend.latest_contents(), b"14");
        assert_eq!(writer.queue_shape(), (0, 0));
    }

    #[tokio::test]
    async fn flush_forces_a_long_debounce_window_immediately() {
        let (backend, _owner, writer) = fixture(
            "flush-immediate",
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::from_secs(120),
        );
        drop(
            writer
                .accept(12, WriteUrgency::Debounced, encode_number)
                .expect("accept long-window snapshot"),
        );

        tokio::time::timeout(Duration::from_millis(500), writer.flush())
            .await
            .expect("flush bypasses debounce")
            .expect("flush persisted");
        assert_eq!(backend.latest_contents(), b"12");
    }

    #[tokio::test]
    async fn immediate_accept_is_not_redelayed_by_a_debounced_replacement() {
        let (backend, _owner, writer) = fixture(
            "sticky-immediate",
            Duration::ZERO,
            Duration::from_secs(60),
            Duration::from_secs(120),
        );
        drop(
            writer
                .accept(1, WriteUrgency::Immediate, encode_number)
                .expect("accept immediate snapshot"),
        );
        let latest = writer
            .accept(2, WriteUrgency::Debounced, encode_number)
            .expect("accept debounced replacement");
        assert!(writer.pending_is_immediate());

        tokio::time::timeout(Duration::from_millis(500), latest.persisted())
            .await
            .expect("sticky immediate deadline")
            .expect("replacement persisted");
        assert_eq!(backend.latest_contents(), b"2");
    }

    #[tokio::test]
    async fn accepted_work_survives_dropping_owner_and_writer_handles() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let gate = backend.gate_next();
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::ZERO,
            Duration::ZERO,
        );
        let root = unique_root("dropped-handles");
        let directory = test_directory(&root);
        let owner = coordinator
            .claim_directory(directory.clone())
            .expect("claim initial owner");
        let destination = directory
            .target(OsStr::new("snapshot.json"), 1024 * 1024)
            .expect("claim initial target");
        let writer = owner.writer(destination).expect("claim initial writer");
        let ticket = writer
            .accept(8, WriteUrgency::Immediate, encode_number)
            .expect("accept snapshot");
        backend.started.notified().await;
        drop(writer);
        drop(owner);
        gate.release();

        ticket.persisted().await.expect("detached write persisted");
        assert_eq!(backend.latest_contents(), b"8");
        let reclaimed = loop {
            match coordinator.claim_directory(directory.clone()) {
                Ok(owner) => break owner,
                Err(PersistenceError::DuplicateOwner) => tokio::task::yield_now().await,
                Err(error) => panic!("unexpected owner reclaim error: {error}"),
            }
        };
        let target = directory
            .target(OsStr::new("snapshot.json"), 1024 * 1024)
            .expect("reclaim exact target");
        reclaimed.writer(target).expect("reclaim writer after worker completion");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn identical_write_retains_generation_against_later_external_replacement() {
        let root = unique_root("identical-write-generation");
        std::fs::create_dir_all(&root).expect("create persistence test root");
        let destination = root.join("snapshot.json");
        std::fs::write(&destination, b"13").expect("seed exact snapshot");
        let directory = test_directory(&root);
        let coordinator = PersistenceCoordinator::for_test(
            Arc::new(FileAtomicWriteBackend),
            Duration::ZERO,
            Duration::ZERO,
        );
        let owner = coordinator
            .claim_directory(directory.clone())
            .expect("claim initial owner");
        let writer = test_writer(&owner, &destination).expect("claim initial writer");

        writer
            .persist(13, encode_number)
            .await
            .expect("identical snapshot accepted");
        assert_eq!(directory.admitted_record_count(), 1);
        drop(writer);
        drop(owner);

        let replacement = root.join("replacement.tmp");
        std::fs::write(&replacement, b"external").expect("write external replacement");
        std::fs::remove_file(&destination).expect("remove admitted namespace binding");
        std::fs::rename(&replacement, &destination).expect("replace admitted generation");

        let reclaimed = tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                match coordinator.claim_directory(directory.clone()) {
                    Ok(owner) => break owner,
                    Err(PersistenceError::DuplicateOwner) => tokio::task::yield_now().await,
                    Err(error) => panic!("unexpected owner reclaim error: {error}"),
                }
            }
        })
        .await
        .expect("owner registration released");
        let writer = test_writer(&reclaimed, &destination).expect("reclaim exact writer");
        assert!(matches!(
            writer.persist(14, encode_number).await,
            Err(PersistenceError::Write {
                kind: io::ErrorKind::AlreadyExists,
                ..
            })
        ));
        assert_eq!(std::fs::read(&destination).expect("read replacement"), b"external");
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn newer_pending_revision_subsumes_failure_and_latest_failure_can_retry() {
        let (backend, _owner, writer) = fixture(
            "retry",
            Duration::from_millis(10),
            Duration::from_millis(5),
            Duration::from_millis(30),
        );
        backend.fail_next();
        let first = writer
            .accept(1, WriteUrgency::Immediate, encode_number)
            .expect("accept first");
        backend.started.notified().await;
        let second = writer
            .accept(2, WriteUrgency::Immediate, encode_number)
            .expect("accept newer");

        assert_eq!(first.persisted().await.expect("subsumed first").get(), 2);
        assert_eq!(second.persisted().await.expect("second persisted").get(), 2);
        assert_eq!(backend.latest_contents(), b"2");
        assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);

        backend.fail_next();
        let failed = writer
            .accept(3, WriteUrgency::Immediate, encode_number)
            .expect("accept failing latest");
        assert!(matches!(
            failed.persisted().await,
            Err(PersistenceError::Write { .. })
        ));
        assert_eq!(
            writer
                .retry()
                .expect("retry latest")
                .persisted()
                .await
                .expect("retry persisted")
                .get(),
            3
        );
        assert_eq!(backend.latest_contents(), b"3");
    }

    #[tokio::test]
    async fn serialization_failure_cannot_retry_but_a_newer_snapshot_can_succeed() {
        let (backend, _owner, writer) = fixture(
            "serialization-failure",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let error = writer
            .persist(1, |_| {
                Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "injected serialization failure",
                ))
            })
            .await
            .expect_err("serialization failure");
        assert!(matches!(error, PersistenceError::Serialization { .. }));
        assert!(matches!(
            writer.retry(),
            Err(PersistenceError::RetryUnavailable)
        ));

        assert_eq!(
            writer
                .persist(2, encode_number)
                .await
                .expect("newer snapshot persisted")
                .get(),
            2
        );
        assert_eq!(backend.latest_contents(), b"2");
    }

    #[tokio::test]
    async fn a_coalesced_write_failure_reaches_every_retained_ticket() {
        let (backend, _owner, writer) = fixture(
            "failure-fanout",
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        backend.fail_next();
        let mut tickets = Vec::new();
        for value in 0..100 {
            tickets.push(
                writer
                    .accept(value, WriteUrgency::Debounced, encode_number)
                    .expect("accept coalesced snapshot"),
            );
        }

        for ticket in tickets {
            assert!(matches!(
                ticket.persisted().await,
                Err(PersistenceError::Write { .. })
            ));
        }
        assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn duplicate_owner_is_rejected_until_every_owner_bound_lane_is_gone() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        let root = unique_root("duplicate-owner");
        let directory = test_directory(&root);
        let owner = coordinator
            .claim_directory(directory.clone())
            .expect("claim owner");
        let writer = test_writer(&owner, &root.join("snapshot.json"))
            .expect("writer");

        assert!(matches!(
            coordinator.claim_directory(directory.clone()),
            Err(PersistenceError::DuplicateOwner)
        ));
        drop(owner);
        assert!(matches!(
            coordinator.claim_directory(directory.clone()),
            Err(PersistenceError::DuplicateOwner)
        ));
        drop(writer);
        coordinator
            .claim_directory(directory)
            .expect("owner released with last lane");
    }

    #[tokio::test]
    async fn successful_close_releases_owner_and_paths_for_immediate_reclaim() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let coordinator = PersistenceCoordinator::for_test(
            backend,
            Duration::from_millis(5),
            Duration::from_millis(20),
        );
        let root = unique_root("close-immediate-reclaim");
        let destination = root.join("snapshot.json");
        let directory = test_directory(&root);
        let mut owner = coordinator
            .claim_directory(directory.clone())
            .expect("claim first owner");

        for revision in 0..128 {
            let writer = test_writer(&owner, &destination)
                .expect("claim snapshot path");
            writer
                .persist(revision, encode_number)
                .await
                .expect("persist before close");
            owner.close().await.expect("close current owner");

            let replacement = coordinator
                .claim_directory(directory.clone())
                .expect("immediately reclaim closed owner root");
            let replacement_writer = test_writer(&replacement, &destination)
                .expect("immediately reclaim closed snapshot path");

            drop(replacement_writer);
            drop(writer);
            drop(owner);
            owner = replacement;
        }

        owner.close().await.expect("close final owner");
    }

    #[tokio::test]
    async fn writer_rejects_capability_scope_and_owner_contract_collisions() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let coordinator = PersistenceCoordinator::for_test(backend, Duration::ZERO, Duration::ZERO);
        let root = unique_root("path-contracts");
        let directory = test_directory(&root);
        let owner = coordinator
            .claim_directory(directory.clone())
            .expect("claim root owner");
        let destination = root.join("status.json");
        let _destination_writer = test_writer(&owner, &destination)
            .expect("claim destination");

        let different_bound = directory
            .target(OsStr::new("status.json"), 512 * 1024)
            .expect("different-bound target");
        assert!(matches!(
            owner.writer(different_bound),
            Err(PersistenceError::TargetMismatch)
        ));
        let alias = directory
            .target(OsStr::new("STATUS.JSON"), 1024 * 1024)
            .expect("alias target");
        assert!(matches!(
            owner.writer(alias),
            Err(PersistenceError::TargetMismatch)
        ));
        let outside = unique_root("outside-capability");
        let outside_target = test_directory(&outside)
            .target(OsStr::new("outside.json"), 1024 * 1024)
            .expect("outside target");
        assert!(matches!(
            owner.writer(outside_target),
            Err(PersistenceError::TargetOutsideOwner)
        ));

        let record = directory
            .target(OsStr::new("other.json"), 1024 * 1024)
            .expect("record owner target");
        assert!(matches!(
            coordinator.claim_record(record),
            Err(PersistenceError::DuplicateOwner)
        ));
    }

    #[tokio::test]
    async fn owner_flushes_and_closes_all_live_child_lanes() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let coordinator = PersistenceCoordinator::for_test(
            backend.clone(),
            Duration::from_millis(10),
            Duration::from_millis(30),
        );
        let root = unique_root("owner-flush");
        let owner = coordinator
            .claim_directory(test_directory(&root))
            .expect("claim owner");
        let first = test_writer(&owner, &root.join("first.json"))
            .expect("first writer");
        let second = test_writer(&owner, &root.join("second.json"))
            .expect("second writer");
        drop(
            first
                .accept(1, WriteUrgency::Debounced, encode_number)
                .expect("accept first"),
        );
        drop(
            second
                .accept(2, WriteUrgency::Debounced, encode_number)
                .expect("accept second"),
        );

        owner.flush().await.expect("flush owner lanes");
        assert_eq!(backend.write_count(), 2);
        owner.close().await.expect("close owner lanes");
        assert!(matches!(
            first.accept(3, WriteUrgency::Immediate, encode_number),
            Err(PersistenceError::Closed)
        ));
        assert!(matches!(
            test_writer(&owner, &root.join("third.json")),
            Err(PersistenceError::Closed)
        ));
    }

    #[tokio::test]
    async fn blocked_physical_write_keeps_ten_thousand_updates_to_one_pending_payload() {
        let (backend, _owner, writer) = fixture(
            "ten-thousand",
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(50),
        );
        let gate = backend.gate_next();
        drop(
            writer
                .accept(0, WriteUrgency::Immediate, encode_number)
                .expect("accept gated snapshot"),
        );
        backend.started.notified().await;
        assert_eq!(writer.queue_shape(), (0, 1));
        let mut latest = None;
        for value in 1..=10_000 {
            latest = Some(
                writer
                    .accept(value, WriteUrgency::Debounced, encode_number)
                    .expect("accept snapshot"),
            );
        }
        assert_eq!(writer.queue_shape(), (1, 1));
        gate.release();
        latest
            .expect("latest ticket")
            .persisted()
            .await
            .expect("latest persisted");

        assert_eq!(backend.latest_contents(), b"10000");
        assert_eq!(backend.write_count(), 2);
        assert_eq!(backend.max_active.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn hard_deadline_writes_during_a_continuous_burst() {
        let (backend, _owner, writer) = fixture(
            "hard-deadline",
            Duration::ZERO,
            Duration::from_millis(30),
            Duration::from_millis(40),
        );
        drop(
            writer
                .accept(0, WriteUrgency::Debounced, encode_number)
                .expect("accept initial"),
        );
        tokio::task::yield_now().await;
        for value in 1..8 {
            tokio::time::advance(Duration::from_millis(5)).await;
            drop(
                writer
                    .accept(value, WriteUrgency::Debounced, encode_number)
                    .expect("accept burst snapshot"),
            );
        }
        assert_eq!(backend.write_count(), 0);
        tokio::time::advance(Duration::from_millis(5)).await;
        backend.started.notified().await;
        writer.flush().await.expect("flush final burst snapshot");
        assert!(backend.write_count() > 0);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn encoder_and_backend_run_off_the_async_runtime_thread() {
        let (backend, _owner, writer) = fixture(
            "blocking-thread",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let runtime_thread = std::thread::current().id();
        let encoder_thread = Arc::new(Mutex::new(None));
        let captured_thread = encoder_thread.clone();
        writer
            .persist(1, move |value| {
                *captured_thread.lock().expect("encoder thread lock") =
                    Some(std::thread::current().id());
                encode_number(value)
            })
            .await
            .expect("persist snapshot");

        let encoder_thread = encoder_thread
            .lock()
            .expect("encoder thread lock")
            .expect("encoder thread");
        let backend_thread = backend.threads.lock().expect("backend thread lock")[0];
        assert_ne!(encoder_thread, runtime_thread);
        assert_ne!(backend_thread, runtime_thread);
        assert_eq!(encoder_thread, backend_thread);
    }

    #[tokio::test]
    async fn failed_owner_close_reopens_for_retry_then_closes_after_success() {
        let (backend, owner, writer) = fixture(
            "owner-close-retry",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        backend.fail_next();
        let gate = backend.gate_next();
        drop(
            writer
                .accept(1, WriteUrgency::Immediate, encode_number)
                .expect("accept closing snapshot"),
        );
        let close_owner = owner.clone();
        let close = tokio::spawn(async move { close_owner.close().await });
        backend.started.notified().await;
        while owner
            .inner
            .state
            .lock()
            .expect("owner state lock")
            .lifecycle
            != OwnerLifecycle::Closing
        {
            tokio::task::yield_now().await;
        }
        assert!(matches!(
            writer.accept(2, WriteUrgency::Immediate, encode_number),
            Err(PersistenceError::Closed)
        ));
        gate.release();
        assert!(close.await.expect("close task").is_err());

        writer
            .retry()
            .expect("retry after failed close")
            .persisted()
            .await
            .expect("retry persisted");
        owner.close().await.expect("successful owner close");
        assert!(matches!(
            writer.accept(3, WriteUrgency::Immediate, encode_number),
            Err(PersistenceError::Closed)
        ));
        assert!(matches!(writer.retry(), Err(PersistenceError::Closed)));
    }

    #[tokio::test]
    async fn cancelled_owner_close_reopens_for_acceptance_and_later_close() {
        let (backend, owner, writer) = fixture(
            "owner-close-cancelled",
            Duration::ZERO,
            Duration::ZERO,
            Duration::ZERO,
        );
        let gate = backend.gate_next();
        drop(
            writer
                .accept(1, WriteUrgency::Immediate, encode_number)
                .expect("accept gated snapshot"),
        );
        let close_owner = owner.clone();
        let close = tokio::spawn(async move { close_owner.close().await });
        backend.started.notified().await;
        while owner
            .inner
            .state
            .lock()
            .expect("owner state lock")
            .lifecycle
            != OwnerLifecycle::Closing
        {
            tokio::task::yield_now().await;
        }

        close.abort();
        assert!(close.await.expect_err("cancel close task").is_cancelled());
        assert!(
            owner
                .inner
                .state
                .lock()
                .expect("owner state lock")
                .lifecycle
                == OwnerLifecycle::Open
        );
        let accepted = writer
            .accept(2, WriteUrgency::Debounced, encode_number)
            .expect("accept after cancelled close");

        gate.release();
        owner.flush().await.expect("flush after cancelled close");
        accepted
            .persisted()
            .await
            .expect("replacement snapshot persisted");
        owner.close().await.expect("successful owner close");
        assert!(matches!(
            writer.accept(3, WriteUrgency::Immediate, encode_number),
            Err(PersistenceError::Closed)
        ));
    }
}
