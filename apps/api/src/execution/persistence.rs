//! Coordinated asynchronous persistence for State-owned snapshots.
//!
//! State decides what is persisted and whether a caller may accept debounce. This
//! module owns process-local lexical path coordination, bounded scheduling,
//! blocking serialization, and atomic file replacement. Atomic replacement gives
//! readers whole-file visibility; it does not fsync data, provide security policy,
//! or coordinate with writers in another process.

use super::file::{FileWriteRequest, atomic_temp_path_for, write_file_atomically};
use crate::state::contracts::TargetDescriptor;
use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, Weak, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;
use tokio::runtime::{Builder, Handle};
use tokio::sync::{Notify, watch};
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
    #[error("failed to normalize persistence path: {message}")]
    PathNormalization {
        kind: io::ErrorKind,
        message: String,
    },
    #[error("persistence owner is already active for this path")]
    DuplicateOwner,
    #[error("persistence destination is outside its owner root")]
    DestinationOutsideOwner,
    #[error("persistence destination is owned by another active store")]
    DestinationOwned,
    #[error("persistence destination was opened with a different target")]
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
            Self::PathNormalization { kind, .. }
            | Self::Serialization { kind, .. }
            | Self::Write { kind, .. } => *kind,
            Self::DuplicateOwner | Self::DestinationOwned | Self::TargetMismatch | Self::Closed => {
                io::ErrorKind::AlreadyExists
            }
            Self::DestinationOutsideOwner => io::ErrorKind::PermissionDenied,
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
        target: &TargetDescriptor,
        destination: &Path,
        contents: &[u8],
    ) -> io::Result<()>;
}

struct FileAtomicWriteBackend;

impl AtomicWriteBackend for FileAtomicWriteBackend {
    fn write(
        &self,
        target: &TargetDescriptor,
        destination: &Path,
        contents: &[u8],
    ) -> io::Result<()> {
        write_file_atomically(FileWriteRequest::new(target.clone(), destination, contents))
            .map(|_| ())
            .map_err(io::Error::from)
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
    owners: Mutex<HashMap<PathBuf, Weak<OwnerInner>>>,
    physical_paths: Mutex<HashMap<PathBuf, Weak<PathLane>>>,
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
                physical_paths: Mutex::new(HashMap::new()),
                backend,
                schedule,
                executor,
            }),
        }
    }

    pub(crate) fn claim_owner(
        &self,
        root: impl AsRef<Path>,
    ) -> Result<PersistenceOwnerLease, PersistenceError> {
        let root = normalize_path(root.as_ref())?;
        let mut owners = self
            .inner
            .owners
            .lock()
            .expect("persistence owner registry lock poisoned");
        owners.retain(|_, owner| owner.strong_count() > 0);
        if owners.get(&root).and_then(Weak::upgrade).is_some() {
            return Err(PersistenceError::DuplicateOwner);
        }
        let owner = Arc::new(OwnerInner {
            root: root.clone(),
            coordinator: self.inner.clone(),
            state: Mutex::new(OwnerState::default()),
        });
        owners.insert(root, Arc::downgrade(&owner));
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
    root: PathBuf,
    coordinator: Arc<CoordinatorInner>,
    state: Mutex<OwnerState>,
}

#[derive(Default)]
struct OwnerState {
    lanes: Vec<Weak<PathLane>>,
    lifecycle: OwnerLifecycle,
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
        self.owner
            .state
            .lock()
            .expect("persistence owner state lock poisoned")
            .lifecycle = if succeeded {
            OwnerLifecycle::Closed
        } else {
            OwnerLifecycle::Open
        };
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
    pub(crate) fn claim(root: impl AsRef<Path>) -> Result<PersistenceOwnerLease, PersistenceError> {
        PersistenceCoordinator::global().claim_owner(root)
    }

    pub(crate) fn writer(
        &self,
        destination: impl AsRef<Path>,
        target: TargetDescriptor,
    ) -> Result<AtomicSnapshotWriter, PersistenceError> {
        let destination = normalize_path(destination.as_ref())?;
        if destination != self.inner.root && !destination.starts_with(&self.inner.root) {
            return Err(PersistenceError::DestinationOutsideOwner);
        }

        let mut owner_state = self
            .inner
            .state
            .lock()
            .expect("persistence owner state lock poisoned");
        if owner_state.lifecycle != OwnerLifecycle::Open {
            return Err(PersistenceError::Closed);
        }

        let temp_path = normalize_path(&atomic_temp_path_for(&destination))?;
        let mut physical_paths = self
            .inner
            .coordinator
            .physical_paths
            .lock()
            .expect("persistence path registry lock poisoned");
        physical_paths.retain(|_, lane| lane.strong_count() > 0);
        if let Some(lane) = physical_paths.get(&destination).and_then(Weak::upgrade) {
            if lane.destination != destination {
                return Err(PersistenceError::DestinationOwned);
            }
            if !Arc::ptr_eq(&lane.owner, &self.inner) {
                return Err(PersistenceError::DestinationOwned);
            }
            if lane.target != target {
                return Err(PersistenceError::TargetMismatch);
            }
            return Ok(AtomicSnapshotWriter { lane });
        }
        for physical_path in [&destination, &temp_path] {
            if physical_paths
                .get(physical_path)
                .and_then(Weak::upgrade)
                .is_some()
            {
                return Err(PersistenceError::DestinationOwned);
            }
        }

        let (progress, _) = watch::channel(CommitProgress::default());
        let lane = Arc::new(PathLane {
            destination: destination.clone(),
            target,
            owner: self.inner.clone(),
            backend: self.inner.coordinator.backend.clone(),
            schedule: self.inner.coordinator.schedule,
            state: Mutex::new(LaneState::default()),
            progress,
            changed: Notify::new(),
            idle: Notify::new(),
        });
        physical_paths.insert(destination, Arc::downgrade(&lane));
        physical_paths.insert(temp_path, Arc::downgrade(&lane));
        owner_state.lanes.retain(|lane| lane.strong_count() > 0);
        owner_state.lanes.push(Arc::downgrade(&lane));
        Ok(AtomicSnapshotWriter { lane })
    }

    pub(crate) async fn flush(&self) -> Result<(), PersistenceError> {
        await_all_lanes(self.live_lanes()).await
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
        let result = await_all_lanes(lanes.clone()).await;
        if result.is_ok() {
            await_all_lanes_idle(&lanes).await;
        }
        let succeeded = result.is_ok();
        transition.finish(succeeded);
        if succeeded {
            self.release_closed_registration();
        }
        result
    }

    fn release_closed_registration(&self) {
        let mut physical_paths = self
            .inner
            .coordinator
            .physical_paths
            .lock()
            .expect("persistence path registry lock poisoned");
        physical_paths.retain(|_, lane| {
            lane.upgrade()
                .is_some_and(|lane| !Arc::ptr_eq(&lane.owner, &self.inner))
        });
        drop(physical_paths);

        let mut owners = self
            .inner
            .coordinator
            .owners
            .lock()
            .expect("persistence owner registry lock poisoned");
        let owns_registration = owners
            .get(&self.inner.root)
            .and_then(Weak::upgrade)
            .is_some_and(|owner| Arc::ptr_eq(&owner, &self.inner));
        if owns_registration {
            owners.remove(&self.inner.root);
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
    let lanes = state
        .lanes
        .iter()
        .filter_map(Weak::upgrade)
        .collect::<Vec<_>>();
    state.lanes.retain(|lane| lane.strong_count() > 0);
    lanes
}

#[derive(Clone)]
pub(crate) struct AtomicSnapshotWriter {
    lane: Arc<PathLane>,
}

struct PathLane {
    destination: PathBuf,
    target: TargetDescriptor,
    owner: Arc<OwnerInner>,
    backend: Arc<dyn AtomicWriteBackend>,
    schedule: PersistenceSchedule,
    state: Mutex<LaneState>,
    progress: watch::Sender<CommitProgress>,
    changed: Notify,
    idle: Notify,
}

#[derive(Default)]
struct LaneState {
    next_revision: u64,
    committed_revision: u64,
    pending: Option<PendingWrite>,
    pending_immediate: bool,
    quiet_deadline: Option<Instant>,
    hard_deadline: Option<Instant>,
    failed_retry: Option<RetryWrite>,
    in_flight_revision: Option<u64>,
    worker_running: bool,
    #[cfg(test)]
    injected_worker_panics: usize,
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
        let (ticket, start_worker) = {
            let owner_state = self
                .lane
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
            state.pending = Some(PendingWrite {
                revision,
                payload: WritePayload::Encode(encoder),
            });
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
            spawn_lane_worker(self.lane.clone());
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
                .lane
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
            spawn_lane_worker(self.lane.clone());
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
            spawn_lane_worker(self.lane.clone());
        } else if notify_worker {
            self.lane.changed.notify_one();
        }
        Ok(self.ticket(revision))
    }

    fn ticket(&self, revision: u64) -> AcceptedWrite {
        AcceptedWrite {
            revision: PersistenceRevision(revision),
            progress: self.lane.progress.subscribe(),
            executor: self.lane.owner.coordinator.executor.clone(),
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

fn spawn_lane_worker(lane: Arc<PathLane>) {
    let executor = lane.owner.coordinator.executor.clone();
    executor.spawn(run_lane(lane));
}

fn restart_lane_worker_if_needed(lane: Arc<PathLane>) {
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
        spawn_lane_worker(lane);
    }
}

struct LaneWorkerGuard {
    lane: Arc<PathLane>,
    armed: bool,
}

impl LaneWorkerGuard {
    fn new(lane: Arc<PathLane>) -> Self {
        Self { lane, armed: true }
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
            spawn_lane_worker(self.lane.clone());
        }
        self.lane.idle.notify_one();
    }
}

async fn run_lane(lane: Arc<PathLane>) {
    let mut guard = LaneWorkerGuard::new(lane.clone());
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
        drop(tokio::task::spawn_blocking(move || {
            let revision = pending.revision;
            let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                run_blocking_write(
                    physical_lane.backend.as_ref(),
                    &physical_lane.target,
                    &physical_lane.destination,
                    pending.payload,
                )
            }))
            .unwrap_or_else(|panic| {
                BlockingWriteOutcome::SerializationFailed(PersistenceError::BlockingTask {
                    message: panic_payload_message(panic),
                })
            });
            complete_blocking_write(physical_lane, revision, outcome);
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

fn complete_blocking_write(lane: Arc<PathLane>, revision: u64, outcome: BlockingWriteOutcome) {
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
    restart_lane_worker_if_needed(lane);
}

enum BlockingWriteOutcome {
    Written,
    SerializationFailed(PersistenceError),
    WriteFailed(PersistenceError, Vec<u8>),
}

fn run_blocking_write(
    backend: &dyn AtomicWriteBackend,
    target: &TargetDescriptor,
    destination: &Path,
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
    match backend.write(target, destination, &contents) {
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

async fn await_all_lanes(lanes: Vec<Arc<PathLane>>) -> Result<(), PersistenceError> {
    let mut first_error = None;
    for lane in lanes {
        if let Err(error) = (AtomicSnapshotWriter { lane }).flush().await
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

fn normalize_path(path: &Path) -> Result<PathBuf, PersistenceError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| PersistenceError::PathNormalization {
                kind: error.kind(),
                message: error.to_string(),
            })?
            .join(path)
    };
    let mut normalized = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                let _ = normalized.pop();
            }
            Component::Normal(value) => normalized.push(value),
        }
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetKind};
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
            _target: &TargetDescriptor,
            destination: &Path,
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
                .push((destination.to_path_buf(), contents.to_vec()));
            self.active.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::State,
            TargetKind::FilesystemPath,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    fn unique_root(name: &str) -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::current_dir()
            .expect("current directory")
            .join("target")
            .join("persistence-tests")
            .join(format!("{name}-{}", NEXT.fetch_add(1, Ordering::Relaxed)))
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
        let owner = coordinator.claim_owner(&root).expect("claim owner");
        let writer = owner
            .writer(root.join("snapshot.json"), target(name))
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
        let owner = PersistenceOwnerLease::claim(&root).expect("claim process owner");
        let writer = owner
            .writer(&destination, target("process-executor"))
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
            .writer(first.lane.destination.clone(), target("serialized"))
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
        let (backend, owner, writer) = fixture(
            "dropped-handles",
            Duration::ZERO,
            Duration::from_millis(20),
            Duration::from_millis(100),
        );
        let ticket = writer
            .accept(8, WriteUrgency::Debounced, encode_number)
            .expect("accept snapshot");
        drop(writer);
        drop(owner);

        ticket.persisted().await.expect("detached write persisted");
        assert_eq!(backend.latest_contents(), b"8");
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
        let owner = coordinator.claim_owner(&root).expect("claim owner");
        let writer = owner
            .writer(root.join("snapshot.json"), target("duplicate-owner"))
            .expect("writer");

        assert!(matches!(
            coordinator.claim_owner(&root),
            Err(PersistenceError::DuplicateOwner)
        ));
        let relative = root
            .strip_prefix(std::env::current_dir().expect("current directory"))
            .expect("test root under current directory");
        assert!(matches!(
            coordinator.claim_owner(relative),
            Err(PersistenceError::DuplicateOwner)
        ));
        assert!(matches!(
            coordinator.claim_owner(root.join("unused").join("..")),
            Err(PersistenceError::DuplicateOwner)
        ));
        drop(owner);
        assert!(matches!(
            coordinator.claim_owner(&root),
            Err(PersistenceError::DuplicateOwner)
        ));
        drop(writer);
        coordinator
            .claim_owner(&root)
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
        let mut owner = coordinator.claim_owner(&root).expect("claim first owner");

        for revision in 0..128 {
            let writer = owner
                .writer(&destination, target("close-immediate-reclaim"))
                .expect("claim snapshot path");
            writer
                .persist(revision, encode_number)
                .await
                .expect("persist before close");
            owner.close().await.expect("close current owner");

            let replacement = coordinator
                .claim_owner(&root)
                .expect("immediately reclaim closed owner root");
            let replacement_writer = replacement
                .writer(&destination, target("close-immediate-reclaim"))
                .expect("immediately reclaim closed snapshot path");

            drop(replacement_writer);
            drop(writer);
            drop(owner);
            owner = replacement;
        }

        owner.close().await.expect("close final owner");
    }

    #[tokio::test]
    async fn writer_rejects_physical_path_and_owner_contract_collisions() {
        let backend = Arc::new(RecordingBackend::new(Duration::ZERO));
        let coordinator = PersistenceCoordinator::for_test(backend, Duration::ZERO, Duration::ZERO);
        let root = unique_root("path-contracts");
        let owner = coordinator.claim_owner(&root).expect("claim root owner");
        let destination = root.join("status.json");
        let _destination_writer = owner
            .writer(&destination, target("path-contracts"))
            .expect("claim destination");

        assert!(matches!(
            owner.writer(&destination, target("different-target")),
            Err(PersistenceError::TargetMismatch)
        ));
        assert!(matches!(
            owner.writer(atomic_temp_path_for(&destination), target("temp-collision")),
            Err(PersistenceError::DestinationOwned)
        ));
        assert!(matches!(
            owner.writer(
                root.parent().expect("root parent").join("outside.json"),
                target("outside")
            ),
            Err(PersistenceError::DestinationOutsideOwner)
        ));

        let nested = root.join("nested");
        let shared_destination = nested.join("shared.json");
        let _shared_writer = owner
            .writer(&shared_destination, target("first-owner"))
            .expect("first owner destination");
        let nested_owner = coordinator
            .claim_owner(&nested)
            .expect("claim overlapping logical owner");
        assert!(matches!(
            nested_owner.writer(&shared_destination, target("second-owner")),
            Err(PersistenceError::DestinationOwned)
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
        let owner = coordinator.claim_owner(&root).expect("claim owner");
        let first = owner
            .writer(root.join("first.json"), target("owner-first"))
            .expect("first writer");
        let second = owner
            .writer(root.join("second.json"), target("owner-second"))
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
            owner.writer(root.join("third.json"), target("owner-third")),
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
