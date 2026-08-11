use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, watch};

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
    parallelism: usize,
}

impl PhysicalWorkRequest {
    pub const fn foreground(io: PhysicalIoClass, scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::Foreground,
            io,
            scratch_bytes,
            parallelism: 1,
        }
    }

    pub const fn foreground_parallel(
        io: PhysicalIoClass,
        scratch_bytes: u64,
        parallelism: usize,
    ) -> Self {
        Self {
            class: PhysicalWorkClass::Foreground,
            io,
            scratch_bytes,
            parallelism,
        }
    }

    pub const fn background(io: PhysicalIoClass, scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::Background,
            io,
            scratch_bytes,
            parallelism: 1,
        }
    }

    pub const fn crash_collection(scratch_bytes: u64) -> Self {
        Self {
            class: PhysicalWorkClass::CrashCollection,
            io: PhysicalIoClass::Read,
            scratch_bytes,
            parallelism: 1,
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
    #[error("physical work capacity is currently unavailable")]
    Unavailable,
    #[error("physical work parallelism exceeds the process worker limit")]
    WorkerLimit,
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
    parallelism: usize,
}

pub struct PhysicalWorkParallelism {
    workers: usize,
    _not_send: std::marker::PhantomData<std::rc::Rc<()>>,
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
    worker_limit: usize,
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
    count: usize,
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
                worker_limit: limits.workers,
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
        validate_parallelism(&self.inner, request.parallelism)?;
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
        let worker = acquire_many(&self.inner.workers, request.parallelism).await?;
        self.inner.active[request.class.index()].fetch_add(1, Ordering::AcqRel);
        Ok(PhysicalWorkAdmission {
            owner: Arc::clone(&self.inner),
            class: request.class,
            _class_permits: class_permits,
            _scratch: scratch,
            _heavy: heavy,
            _worker: worker,
            parallelism: request.parallelism,
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

    pub fn try_run_inline<T, Work>(
        &self,
        request: PhysicalWorkRequest,
        work: Work,
    ) -> Result<T, PhysicalWorkError>
    where
        Work: FnOnce() -> T,
    {
        let admission = self.try_admit(request)?;
        let _running = RunningWorker::new(&admission);
        let _admission = admission;
        Ok(work())
    }

    fn try_admit(
        &self,
        request: PhysicalWorkRequest,
    ) -> Result<PhysicalWorkAdmission, PhysicalWorkError> {
        validate_parallelism(&self.inner, request.parallelism)?;
        let mut class_permits = Vec::with_capacity(2);
        if matches!(
            request.class,
            PhysicalWorkClass::Background | PhysicalWorkClass::CrashCollection
        ) {
            class_permits.push(try_acquire_one(&self.inner.background)?);
        }
        if request.class == PhysicalWorkClass::CrashCollection {
            class_permits.push(try_acquire_one(&self.inner.crash)?);
        }
        let scratch = self.try_reserve_scratch_inner(request.scratch_bytes)?;
        let heavy = if request.io == PhysicalIoClass::Heavy {
            Some(try_acquire_one(&self.inner.heavy)?)
        } else {
            None
        };
        let worker = try_acquire_many(&self.inner.workers, request.parallelism)?;
        self.inner.active[request.class.index()].fetch_add(1, Ordering::AcqRel);
        Ok(PhysicalWorkAdmission {
            owner: Arc::clone(&self.inner),
            class: request.class,
            _class_permits: class_permits,
            _scratch: scratch,
            _heavy: heavy,
            _worker: worker,
            parallelism: request.parallelism,
        })
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

    fn try_reserve_scratch_inner(
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
            .try_acquire_many_owned(units)
            .map(Some)
            .map_err(map_try_acquire_error)
    }
}

impl PhysicalWorkAdmission {
    pub async fn run<T, Work>(self, work: Work) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation) -> T + Send + 'static,
    {
        self.run_parallel(move |cancellation, _parallelism| work(cancellation))
            .await
    }

    pub async fn run_parallel<T, Work>(self, work: Work) -> Result<T, PhysicalWorkError>
    where
        T: Send + 'static,
        Work: FnOnce(PhysicalWorkCancellation, PhysicalWorkParallelism) -> T + Send + 'static,
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
            let parallelism = PhysicalWorkParallelism {
                workers: self.parallelism,
                _not_send: std::marker::PhantomData,
            };
            let _admission = self;
            if cancellation.is_cancelled() {
                PhysicalWorkOutput::Cancelled
            } else {
                PhysicalWorkOutput::Complete(work(cancellation, parallelism))
            }
        });
        let result = task.await;
        cancel_on_drop.armed = false;
        map_task_result(result)
    }
}

impl PhysicalWorkParallelism {
    pub const fn workers(&self) -> usize {
        self.workers
    }

    pub fn run_scoped<T, Work>(&self, jobs: Vec<Work>) -> Result<Vec<T>, PhysicalWorkError>
    where
        T: Send,
        Work: FnOnce() -> T + Send,
    {
        if jobs.len() > self.workers {
            return Err(PhysicalWorkError::WorkerLimit);
        }
        let job_count = jobs.len();
        std::thread::scope(|scope| {
            let mut failed = false;
            let mut workers = Vec::with_capacity(jobs.len());
            for (index, job) in jobs.into_iter().enumerate() {
                match std::thread::Builder::new()
                    .name(format!("axial-physical-{index}"))
                    .spawn_scoped(scope, job)
                {
                    Ok(worker) => workers.push((index, worker)),
                    Err(_) => failed = true,
                }
            }
            let mut results = std::iter::repeat_with(|| None)
                .take(job_count)
                .collect::<Vec<_>>();
            for (index, worker) in workers {
                match worker.join() {
                    Ok(result) => results[index] = Some(result),
                    Err(_) => failed = true,
                }
            }
            if failed {
                Err(PhysicalWorkError::TaskStopped)
            } else {
                results
                    .into_iter()
                    .collect::<Option<Vec<_>>>()
                    .ok_or(PhysicalWorkError::TaskStopped)
            }
        })
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
        admission.owner.running[admission.class.index()]
            .fetch_add(admission.parallelism, Ordering::AcqRel);
        Self {
            owner: Arc::clone(&admission.owner),
            class: admission.class,
            count: admission.parallelism,
        }
    }
}

impl Drop for RunningWorker {
    fn drop(&mut self) {
        self.owner.running[self.class.index()].fetch_sub(self.count, Ordering::AcqRel);
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

async fn acquire_many(
    gate: &Arc<Semaphore>,
    count: usize,
) -> Result<OwnedSemaphorePermit, PhysicalWorkError> {
    let count = u32::try_from(count).map_err(|_| PhysicalWorkError::WorkerLimit)?;
    Arc::clone(gate)
        .acquire_many_owned(count)
        .await
        .map_err(|_| PhysicalWorkError::Closed)
}

fn try_acquire_one(gate: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, PhysicalWorkError> {
    Arc::clone(gate)
        .try_acquire_owned()
        .map_err(map_try_acquire_error)
}

fn try_acquire_many(
    gate: &Arc<Semaphore>,
    count: usize,
) -> Result<OwnedSemaphorePermit, PhysicalWorkError> {
    let count = u32::try_from(count).map_err(|_| PhysicalWorkError::WorkerLimit)?;
    Arc::clone(gate)
        .try_acquire_many_owned(count)
        .map_err(map_try_acquire_error)
}

fn validate_parallelism(
    owner: &PhysicalWorkOwnerInner,
    parallelism: usize,
) -> Result<(), PhysicalWorkError> {
    if parallelism == 0 || parallelism > owner.worker_limit {
        Err(PhysicalWorkError::WorkerLimit)
    } else {
        Ok(())
    }
}

fn map_try_acquire_error(error: TryAcquireError) -> PhysicalWorkError {
    match error {
        TryAcquireError::Closed => PhysicalWorkError::Closed,
        TryAcquireError::NoPermits => PhysicalWorkError::Unavailable,
    }
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

    #[test]
    fn inline_work_is_counted_and_refuses_excess_capacity() {
        let owner = test_owner();
        let nested_owner = owner.clone();
        owner
            .try_run_inline(
                PhysicalWorkRequest::foreground(PhysicalIoClass::Read, SCRATCH_UNIT_BYTES),
                move || {
                    let snapshot = nested_owner.snapshot(PhysicalWorkClass::Foreground);
                    assert_eq!(snapshot.active_admissions, 1);
                    assert_eq!(snapshot.running_workers, 1);

                    let second_owner = nested_owner.clone();
                    nested_owner
                        .try_run_inline(
                            PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                            move || {
                                second_owner
                                    .try_run_inline(
                                        PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                                        || {
                                            assert_eq!(
                                                second_owner.try_run_inline(
                                                    PhysicalWorkRequest::foreground(
                                                        PhysicalIoClass::Read,
                                                        0,
                                                    ),
                                                    || (),
                                                ),
                                                Err(PhysicalWorkError::Unavailable)
                                            );
                                        },
                                    )
                                    .expect("third worker");
                            },
                        )
                        .expect("second worker");
                },
            )
            .expect("inline admission");
        let snapshot = owner.snapshot(PhysicalWorkClass::Foreground);
        assert_eq!(snapshot.active_admissions, 0);
        assert_eq!(snapshot.running_workers, 0);
    }

    #[tokio::test]
    async fn parallel_admission_reserves_every_worker_and_restores_job_order() {
        let owner = test_owner();
        let admission = owner
            .admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Metadata,
                0,
                3,
            ))
            .await
            .expect("parallel admission");
        assert_eq!(
            owner
                .snapshot(PhysicalWorkClass::Foreground)
                .available_workers,
            0
        );
        let observed_owner = owner.clone();
        let results = admission
            .run_parallel(move |_, parallelism| {
                assert_eq!(parallelism.workers(), 3);
                assert_eq!(
                    observed_owner
                        .snapshot(PhysicalWorkClass::Foreground)
                        .running_workers,
                    3
                );
                parallelism.run_scoped((0..3).map(|index| move || index).collect())
            })
            .await
            .expect("parallel worker")
            .expect("scoped jobs");
        assert_eq!(results, vec![0, 1, 2]);
        let snapshot = owner.snapshot(PhysicalWorkClass::Foreground);
        assert_eq!(snapshot.active_admissions, 0);
        assert_eq!(snapshot.running_workers, 0);
        assert_eq!(snapshot.available_workers, 3);
    }

    #[tokio::test]
    async fn parallel_admission_rejects_zero_and_excess_workers() {
        let owner = test_owner();
        for parallelism in [0, 4] {
            assert!(matches!(
                owner
                    .admit(PhysicalWorkRequest::foreground_parallel(
                        PhysicalIoClass::Metadata,
                        0,
                        parallelism,
                    ))
                    .await,
                Err(PhysicalWorkError::WorkerLimit)
            ));
        }
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
