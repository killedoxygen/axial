use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, watch};

const PROCESS_WORKER_LIMIT: usize = 4;
const BACKGROUND_WORKER_LIMIT: usize = 2;
const CRASH_COLLECTION_LIMIT: usize = 2;
const HEAVY_IO_LIMIT: usize = 1;
const SCRATCH_UNIT_BYTES: u64 = 1 << 20;
const SCRATCH_LIMIT_BYTES: u64 = 512 << 20;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalWorkClass {
    Foreground,
    Background,
    CrashCollection,
}

impl PhysicalWorkClass {
    const fn index(self) -> usize {
        match self {
            Self::Foreground => 0,
            Self::Background => 1,
            Self::CrashCollection => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalIoClass {
    Metadata,
    Read,
    Write,
    Heavy,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalWorkRequest {
    class: PhysicalWorkClass,
    io: PhysicalIoClass,
    scratch_bytes: u64,
}

impl PhysicalWorkRequest {
    pub const fn foreground(io: PhysicalIoClass, scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::Foreground,
            io,
            scratch_bytes,
        }
    }

    pub const fn background(io: PhysicalIoClass, scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::Background,
            io,
            scratch_bytes,
        }
    }

    pub const fn crash_collection(scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::CrashCollection,
            io: PhysicalIoClass::Read,
            scratch_bytes,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PhysicalWorkSnapshot {
    pub active_admissions: usize,
    pub running_workers: usize,
    pub available_workers: usize,
    pub available_scratch_bytes: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub enum PhysicalWorkError {
    #[error("physical work admission is closed")]
    Closed,
    #[error("physical work scratch request exceeds the process limit")]
    ScratchLimit,
    #[error("physical work was cancelled")]
    Cancelled,
    #[error("physical work exceeded its deadline")]
    Deadline,
    #[error("physical worker stopped before returning a result")]
    TaskStopped,
}

#[derive(Clone)]
pub struct PhysicalWorkOwner {
    inner: Arc<PhysicalWorkOwnerInner>,
}

pub struct PhysicalWorkAdmission {
    owner: Arc<PhysicalWorkOwnerInner>,
    class: PhysicalWorkClass,
    _class_permits: Vec<OwnedSemaphorePermit>,
    _scratch: Option<OwnedSemaphorePermit>,
    _heavy: Option<OwnedSemaphorePermit>,
    _worker: OwnedSemaphorePermit,
}

pub struct PhysicalScratchPermit {
    _permit: OwnedSemaphorePermit,
}

#[derive(Clone)]
pub struct PhysicalWorkGroup {
    owner: PhysicalWorkOwner,
    inner: Arc<PhysicalWorkGroupInner>,
}

#[derive(Clone)]
pub struct PhysicalWorkCancellation {
    group_cancelled: Option<Arc<AtomicBool>>,
    task_cancelled: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

struct PhysicalWorkOwnerInner {
    workers: Arc<Semaphore>,
    background: Arc<Semaphore>,
    crash: Arc<Semaphore>,
    heavy: Arc<Semaphore>,
    scratch: Arc<Semaphore>,
    scratch_limit_bytes: u64,
    active: [AtomicUsize; 3],
    running: [AtomicUsize; 3],
}

struct PhysicalWorkGroupInner {
    cancelled: Arc<AtomicBool>,
    cancellation: watch::Sender<bool>,
    active_count: AtomicUsize,
    active: watch::Sender<usize>,
}

struct PhysicalWorkGroupRegistration {
    inner: Arc<PhysicalWorkGroupInner>,
}

struct RunningWorker {
    owner: Arc<PhysicalWorkOwnerInner>,
    class: PhysicalWorkClass,
}

struct CancelOnDrop {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

enum PhysicalWorkOutput<T> {
    Cancelled,
    Complete(T),
}

pub fn process_physical_work() -> PhysicalWorkOwner {
    static OWNER: OnceLock<PhysicalWorkOwner> = OnceLock::new();
    OWNER
        .get_or_init(|| {
            PhysicalWorkOwner::new(PhysicalWorkLimits {
                workers: PROCESS_WORKER_LIMIT,
                background: BACKGROUND_WORKER_LIMIT,
                crash: CRASH_COLLECTION_LIMIT,
                heavy: HEAVY_IO_LIMIT,
                scratch_bytes: SCRATCH_LIMIT_BYTES,
            })
        })
        .clone()
}

impl PhysicalWorkOwner {
    fn new(limits: PhysicalWorkLimits) -> Self {
        let scratch_units = limits.scratch_bytes.div_ceil(SCRATCH_UNIT_BYTES);
        let scratch_units = usize::try_from(scratch_units).expect("scratch unit count fits usize");
        Self {
            inner: Arc::new(PhysicalWorkOwnerInner {
                workers: Arc::new(Semaphore::new(limits.workers)),
                background: Arc::new(Semaphore::new(limits.background)),
                crash: Arc::new(Semaphore::new(limits.crash)),
                heavy: Arc::new(Semaphore::new(limits.heavy)),
                scratch: Arc::new(Semaphore::new(scratch_units)),
                scratch_limit_bytes: limits.scratch_bytes,
                active: std::array::from_fn(|_| AtomicUsize::new(0)),
                running: std::array::from_fn(|_| AtomicUsize::new(0)),
            }),
        }
    }

    pub fn group(&self) -> PhysicalWorkGroup {
        PhysicalWorkGroup {
            owner: self.clone(),
            inner: Arc::new(PhysicalWorkGroupInner {
                cancelled: Arc::new(AtomicBool::new(false)),
                cancellation: watch::channel(false).0,
                active_count: AtomicUsize::new(0),
                active: watch::channel(0).0,
            }),
        }
    }

    pub async fn admit(
        &self,
        request: PhysicalWorkRequest,
    ) -> Result<PhysicalWorkAdmission, PhysicalWorkError> {
        let mut class_permits = Vec::with_capacity(2);
        if matches!(
            request.class,
            PhysicalWorkClass::Background | PhysicalWorkClass::CrashCollection
        ) {
            class_permits.push(acquire_one(&self.inner.background).await?);
        }
        if request.class == PhysicalWorkClass::CrashCollection {
            class_permits.push(acquire_one(&self.inner.crash).await?);
        }
        let scratch = self.reserve_scratch_inner(request.scratch_bytes).await?;
        let heavy = if request.io == PhysicalIoClass::Heavy {
            Some(acquire_one(&self.inner.heavy).await?)
        } else {
            None
        };
        let worker = acquire_one(&self.inner.workers).await?;
        self.inner.active[request.class.index()].fetch_add(1, Ordering::AcqRel);
        Ok(PhysicalWorkAdmission {
            owner: Arc::clone(&self.inner),
            class: request.class,
            _class_permits: class_permits,
            _scratch: scratch,
            _heavy: heavy,
            _worker: worker,
        })
    }

    pub async fn reserve_scratch(
        &self,
        bytes: u64,
    ) -> Result<Option<PhysicalScratchPermit>, PhysicalWorkError> {
        Ok(self
            .reserve_scratch_inner(bytes)
            .await?
            .map(|permit| PhysicalScratchPermit { _permit: permit }))
    }

    pub fn snapshot(&self, class: PhysicalWorkClass) -> PhysicalWorkSnapshot {
        PhysicalWorkSnapshot {
            active_admissions: self.inner.active[class.index()].load(Ordering::Acquire),
            running_workers: self.inner.running[class.index()].load(Ordering::Acquire),
            available_workers: self.inner.workers.available_permits(),
            available_scratch_bytes: self.inner.scratch.available_permits() as u64
                * SCRATCH_UNIT_BYTES,
        }
    }

    pub fn scratch_limit_bytes(&self) -> u64 {
        self.inner.scratch_limit_bytes
    }

    async fn reserve_scratch_inner(
        &self,
        bytes: u64,
    ) -> Result<Option<OwnedSemaphorePermit>, PhysicalWorkError> {
        if bytes > self.inner.scratch_limit_bytes {
            return Err(PhysicalWorkError::ScratchLimit);
        }
        if bytes == 0 {
            return Ok(None);
        }
        let units = bytes.div_ceil(SCRATCH_UNIT_BYTES);
        let units = u32::try_from(units).map_err(|_| PhysicalWorkError::ScratchLimit)?;
        Arc::clone(&self.inner.scratch)
            .acquire_many_owned(units)
            .await
            .map(Some)
            .map_err(|_| PhysicalWorkError::Closed)
    }
}

impl PhysicalWorkAdmission {
    pub async fn run<T, Work>(self, work: Work) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation) -> T + Send + 'static,
    {
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut cancel_on_drop = CancelOnDrop {
            cancelled: Arc::clone(&cancelled),
            armed: true,
        };
        let cancellation = PhysicalWorkCancellation {
            group_cancelled: None,
            task_cancelled: cancelled,
            deadline: None,
        };
        let task = tokio::task::spawn_blocking(move || {
            let _running = RunningWorker::new(&self);
            let _admission = self;
            if cancellation.is_cancelled() {
                PhysicalWorkOutput::Cancelled
            } else {
                PhysicalWorkOutput::Complete(work(cancellation))
            }
        });
        let result = task.await;
        cancel_on_drop.armed = false;
        map_task_result(result)
    }
}

impl PhysicalWorkGroup {
    pub async fn run<T, Work>(
        &self,
        request: PhysicalWorkRequest,
        work: Work,
    ) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation) -> T + Send + 'static,
    {
        self.run_inner(request, None, work).await
    }

    pub async fn run_until<T, Work>(
        &self,
        request: PhysicalWorkRequest,
        deadline: Duration,
        work: Work,
    ) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation) -> T + Send + 'static,
    {
        self.run_inner(request, Some(deadline), work).await
    }

    pub fn cancel(&self) {
        self.inner.cancelled.store(true, Ordering::Release);
        self.inner.cancellation.send_replace(true);
    }

    pub fn active(&self) -> usize {
        self.inner.active_count.load(Ordering::Acquire)
    }

    pub async fn drain(&self) {
        let mut active = self.inner.active.subscribe();
        loop {
            active.borrow_and_update();
            if self.inner.active_count.load(Ordering::Acquire) == 0 {
                return;
            }
            active
                .changed()
                .await
                .expect("physical work group counter remains owned");
        }
    }

    pub async fn drain_until(&self, deadline: Duration) -> bool {
        tokio::time::timeout(deadline, self.drain()).await.is_ok()
    }

    async fn run_inner<T, Work>(
        &self,
        request: PhysicalWorkRequest,
        deadline: Option<Duration>,
        work: Work,
    ) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation) -> T + Send + 'static,
    {
        let blocking_deadline = deadline.and_then(|value| Instant::now().checked_add(value));
        let async_deadline = deadline.map(|value| tokio::time::Instant::now() + value);
        let registration = self.register()?;
        let mut cancelled = self.inner.cancellation.subscribe();
        let admission = if let Some(deadline) = async_deadline {
            tokio::select! {
                admission = self.owner.admit(request) => admission?,
                changed = cancelled.changed() => {
                    let _ = changed;
                    return Err(PhysicalWorkError::Cancelled);
                }
                _ = tokio::time::sleep_until(deadline) => {
                    return Err(PhysicalWorkError::Deadline);
                }
            }
        } else {
            tokio::select! {
                admission = self.owner.admit(request) => admission?,
                changed = cancelled.changed() => {
                    let _ = changed;
                    return Err(PhysicalWorkError::Cancelled);
                }
            }
        };
        let task_cancelled = Arc::new(AtomicBool::new(false));
        let mut cancel_on_drop = CancelOnDrop {
            cancelled: Arc::clone(&task_cancelled),
            armed: true,
        };
        let cancellation = PhysicalWorkCancellation {
            group_cancelled: Some(Arc::clone(&self.inner.cancelled)),
            task_cancelled,
            deadline: blocking_deadline,
        };
        let task = tokio::task::spawn_blocking(move || {
            let _registration = registration;
            let _running = RunningWorker::new(&admission);
            let _admission = admission;
            if cancellation.is_cancelled() {
                PhysicalWorkOutput::Cancelled
            } else {
                PhysicalWorkOutput::Complete(work(cancellation))
            }
        });
        let result = if let Some(deadline) = async_deadline {
            match tokio::time::timeout_at(deadline, task).await {
                Ok(result) => map_task_result(result),
                Err(_) => Err(PhysicalWorkError::Deadline),
            }
        } else {
            map_task_result(task.await)
        };
        if !matches!(result, Err(PhysicalWorkError::Deadline)) {
            cancel_on_drop.armed = false;
        }
        result
    }

    fn register(&self) -> Result<PhysicalWorkGroupRegistration, PhysicalWorkError> {
        if self.inner.cancelled.load(Ordering::Acquire) {
            return Err(PhysicalWorkError::Cancelled);
        }
        let active = increment_group_count(&self.inner.active_count);
        self.inner.active.send_replace(active);
        if self.inner.cancelled.load(Ordering::Acquire) {
            let active = decrement_group_count(&self.inner.active_count);
            self.inner.active.send_replace(active);
            return Err(PhysicalWorkError::Cancelled);
        }
        Ok(PhysicalWorkGroupRegistration {
            inner: Arc::clone(&self.inner),
        })
    }
}

impl PhysicalWorkCancellation {
    pub fn is_cancelled(&self) -> bool {
        self.task_cancelled.load(Ordering::Acquire)
            || self
                .group_cancelled
                .as_ref()
                .is_some_and(|cancelled| cancelled.load(Ordering::Acquire))
            || self
                .deadline
                .is_some_and(|deadline| Instant::now() >= deadline)
    }

    pub fn check(&self) -> Result<(), PhysicalWorkError> {
        if self.is_cancelled() {
            Err(PhysicalWorkError::Cancelled)
        } else {
            Ok(())
        }
    }
}

impl RunningWorker {
    fn new(admission: &PhysicalWorkAdmission) -> Self {
        admission.owner.running[admission.class.index()].fetch_add(1, Ordering::AcqRel);
        Self {
            owner: Arc::clone(&admission.owner),
            class: admission.class,
        }
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        self.owner.running[self.class.index()].fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for PhysicalWorkAdmission {
    fn drop(&mut self) {
        self.owner.active[self.class.index()].fetch_sub(1, Ordering::AcqRel);
    }
}

impl Drop for PhysicalWorkGroupRegistration {
    fn drop(&mut self) {
        let active = decrement_group_count(&self.inner.active_count);
        self.inner.active.send_replace(active);
    }
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

async fn acquire_one(gate: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, PhysicalWorkError> {
    Arc::clone(gate)
        .acquire_owned()
        .await
        .map_err(|_| PhysicalWorkError::Closed)
}

fn increment_group_count(active: &AtomicUsize) -> usize {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_add(1)
        })
        .expect("physical work group count overflowed")
        + 1
}

fn decrement_group_count(active: &AtomicUsize) -> usize {
    active
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
            value.checked_sub(1)
        })
        .expect("physical work group count underflowed")
        - 1
}

fn map_task_result<T>(
    result: Result<PhysicalWorkOutput<T>, tokio::task::JoinError>,
) -> Result<T, PhysicalWorkError> {
    match result {
        Ok(PhysicalWorkOutput::Complete(value)) => Ok(value),
        Ok(PhysicalWorkOutput::Cancelled) => Err(PhysicalWorkError::Cancelled),
        Err(_) => Err(PhysicalWorkError::TaskStopped),
    }
}

#[derive(Clone, Copy)]
struct PhysicalWorkLimits {
    workers: usize,
    background: usize,
    crash: usize,
    heavy: usize,
    scratch_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    fn test_owner() -> PhysicalWorkOwner {
        PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 3,
            background: 2,
            crash: 2,
            heavy: 1,
            scratch_bytes: 2 * SCRATCH_UNIT_BYTES,
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p01_b04_contract_deadline_retains_capacity_until_physical_exit() {
        let owner = test_owner();
        let stalled = owner.group();
        let other = owner.group();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_worker = Arc::clone(&gate);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let result = stalled
            .run_until(
                PhysicalWorkRequest::crash_collection(SCRATCH_UNIT_BYTES),
                Duration::from_millis(20),
                move |cancellation| {
                    let _ = started_tx.send(());
                    let (lock, wake) = &*gate_for_worker;
                    let released = lock.lock().expect("lock stalled worker");
                    drop(
                        wake.wait_while(released, |released| !*released)
                            .expect("wait for worker release"),
                    );
                    assert!(cancellation.is_cancelled());
                },
            )
            .await;
        started_rx.await.expect("stalled worker started");
        assert_eq!(result, Err(PhysicalWorkError::Deadline));
        assert_eq!(stalled.active(), 1);
        assert_eq!(
            owner
                .snapshot(PhysicalWorkClass::CrashCollection)
                .active_admissions,
            1
        );

        assert_eq!(
            other
                .run(PhysicalWorkRequest::crash_collection(0), |_| 7_u8)
                .await,
            Ok(7)
        );
        assert_eq!(stalled.active(), 1);

        let (lock, wake) = &*gate;
        *lock.lock().expect("release stalled worker") = true;
        wake.notify_one();
        tokio::time::timeout(Duration::from_secs(2), stalled.drain())
            .await
            .expect("stalled worker exits");
        assert_eq!(
            owner
                .snapshot(PhysicalWorkClass::CrashCollection)
                .active_admissions,
            0
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p01_b04_contract_cross_owner_reserves_foreground_and_scratch() {
        let owner = PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 2,
            background: 1,
            crash: 1,
            heavy: 1,
            scratch_bytes: 2 * SCRATCH_UNIT_BYTES,
        });
        let first = owner.group();
        let second = owner.group();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_worker = Arc::clone(&gate);
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let first_task = tokio::spawn({
            let first = first.clone();
            async move {
                first
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                        move |_| {
                            let _ = started_tx.send(());
                            let (lock, wake) = &*gate_for_worker;
                            let released = lock.lock().expect("lock background worker");
                            drop(
                                wake.wait_while(released, |released| !*released)
                                    .expect("wait for background release"),
                            );
                        },
                    )
                    .await
            }
        });
        started_rx.await.expect("background worker started");

        let queued = tokio::spawn({
            let second = second.clone();
            async move {
                second
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                        |_| (),
                    )
                    .await
            }
        });
        tokio::task::yield_now().await;
        assert_eq!(
            owner
                .admit(PhysicalWorkRequest::foreground(
                    PhysicalIoClass::Metadata,
                    0,
                ))
                .await
                .expect("foreground remains admitted")
                .run(|_| 9_u8)
                .await,
            Ok(9)
        );
        second.cancel();
        assert_eq!(
            queued.await.expect("join queued work"),
            Err(PhysicalWorkError::Cancelled)
        );

        let scratch = owner
            .reserve_scratch(2 * SCRATCH_UNIT_BYTES)
            .await
            .expect("reserve all scratch")
            .expect("nonzero scratch permit");
        assert_eq!(
            owner
                .snapshot(PhysicalWorkClass::Foreground)
                .available_scratch_bytes,
            0
        );
        drop(scratch);
        assert!(owner.reserve_scratch(SCRATCH_UNIT_BYTES).await.is_ok());

        let (lock, wake) = &*gate;
        *lock.lock().expect("release background worker") = true;
        wake.notify_one();
        assert_eq!(first_task.await.expect("join first work"), Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn p01_b04_contract_queue_wait_is_inside_the_deadline() {
        let owner = PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 2,
            background: 2,
            crash: 2,
            heavy: 1,
            scratch_bytes: 2 * SCRATCH_UNIT_BYTES,
        });
        let active = owner.group();
        let queued = owner.group();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let mut tasks = Vec::new();
        let mut starts = Vec::new();
        for _ in 0..2 {
            let active = active.clone();
            let gate = Arc::clone(&gate);
            let (started_tx, started_rx) = tokio::sync::oneshot::channel();
            starts.push(started_rx);
            tasks.push(tokio::spawn(async move {
                active
                    .run(PhysicalWorkRequest::crash_collection(0), move |_| {
                        let _ = started_tx.send(());
                        let (lock, wake) = &*gate;
                        let released = lock.lock().expect("lock crash worker");
                        drop(
                            wake.wait_while(released, |released| !*released)
                                .expect("wait for crash release"),
                        );
                    })
                    .await
            }));
        }
        for started in starts {
            started.await.expect("crash worker started");
        }

        let starts = Arc::new(AtomicUsize::new(0));
        let starts_in_work = Arc::clone(&starts);
        assert_eq!(
            queued
                .run_until(
                    PhysicalWorkRequest::crash_collection(0),
                    Duration::from_millis(20),
                    move |_| {
                        starts_in_work.fetch_add(1, Ordering::Relaxed);
                    },
                )
                .await,
            Err(PhysicalWorkError::Deadline)
        );
        assert_eq!(starts.load(Ordering::Relaxed), 0);
        assert_eq!(queued.active(), 0);

        let (lock, wake) = &*gate;
        *lock.lock().expect("release crash workers") = true;
        wake.notify_all();
        for task in tasks {
            assert_eq!(task.await.expect("join crash worker"), Ok(()));
        }
    }
}
