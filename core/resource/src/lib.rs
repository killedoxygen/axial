use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, TryAcquireError, watch};

const PROCESS_WORKER_LIMIT: usize = 4;
const BACKGROUND_WORKER_LIMIT: usize = 2;
const HEAVY_IO_LIMIT: usize = 1;
const SCRATCH_UNIT_BYTES: u64 = 1 << 20;
const SCRATCH_LIMIT_BYTES: u64 = 512 << 20;
/// Background workers admitted at once while any host activity (a running
/// game) holds a lease. Foreground, recovery and shutdown work is unaffected.
const BACKGROUND_DURING_ACTIVITY: usize = 1;
/// Concurrent activities one process may record.
const MAX_ACTIVITIES: usize = 64;
const PROCESS_LIMITS: PhysicalWorkLimits = PhysicalWorkLimits {
    workers: PROCESS_WORKER_LIMIT,
    background: BACKGROUND_WORKER_LIMIT,
    heavy: HEAVY_IO_LIMIT,
    scratch_bytes: SCRATCH_LIMIT_BYTES,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PhysicalWorkClass {
    Foreground,
    Background,
}

impl PhysicalWorkClass {
    const fn index(self) -> usize {
        match self {
            Self::Foreground => 0,
            Self::Background => 1,
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
    #[error("physical worker stopped before returning a result")]
    TaskStopped,
}

/// Owner-neutral record of what the host is running: how many activities
/// and the memory they reserved. A revision orders every change.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ActivitySnapshot {
    pub count: usize,
    pub reserved_mib: u64,
    pub revision: u64,
}

/// One recorded activity; dropping it releases its count and reservation.
pub struct ActivityLease {
    owner: Arc<PhysicalWorkOwnerInner>,
    reserved_mib: u64,
}

impl ActivityLease {
    pub fn reserved_mib(&self) -> u64 {
        self.reserved_mib
    }
}

impl std::fmt::Debug for ActivityLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivityLease")
            .field("reserved_mib", &self.reserved_mib)
            .finish()
    }
}

impl Drop for ActivityLease {
    fn drop(&mut self) {
        let mut ledger = self
            .owner
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        // Admission recorded this lease's count and reservation exactly and
        // kept a revision for its release.
        ledger.count -= 1;
        ledger.reserved_mib -= self.reserved_mib;
        ledger.revision += 1;
        let revision = ledger.revision;
        drop(ledger);
        self.owner.activity_changed.send_replace(revision);
    }
}

#[derive(Clone)]
pub struct PhysicalWorkOwner {
    inner: Arc<PhysicalWorkOwnerInner>,
}

pub struct PhysicalWorkAdmission {
    owner: Arc<PhysicalWorkOwnerInner>,
    class: PhysicalWorkClass,
    _background: Option<OwnedSemaphorePermit>,
    _quieted: Option<OwnedSemaphorePermit>,
    _scratch: Option<OwnedSemaphorePermit>,
    _heavy: Option<OwnedSemaphorePermit>,
    _worker: OwnedSemaphorePermit,
    parallelism: usize,
}

/// The workers one admission lends to the calling thread's scoped jobs.
pub struct PhysicalWorkParallelism {
    owner: Arc<PhysicalWorkOwnerInner>,
    class: PhysicalWorkClass,
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
}

struct PhysicalWorkOwnerInner {
    workers: Arc<Semaphore>,
    worker_limit: usize,
    background: Arc<Semaphore>,
    background_during_activity: Arc<Semaphore>,
    activity: std::sync::Mutex<ActivitySnapshot>,
    activity_changed: watch::Sender<u64>,
    heavy: Arc<Semaphore>,
    scratch: Arc<Semaphore>,
    scratch_limit_bytes: u64,
    active: [AtomicUsize; 2],
    running: [AtomicUsize; 2],
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
        .get_or_init(|| PhysicalWorkOwner::new(PROCESS_LIMITS))
        .clone()
}

impl PhysicalWorkOwner {
    #[cfg(feature = "test-support")]
    pub fn isolated_for_test() -> Self {
        Self::new(PROCESS_LIMITS)
    }

    fn new(limits: PhysicalWorkLimits) -> Self {
        let scratch_units = limits.scratch_bytes.div_ceil(SCRATCH_UNIT_BYTES);
        let scratch_units = usize::try_from(scratch_units).expect("scratch unit count fits usize");
        Self {
            inner: Arc::new(PhysicalWorkOwnerInner {
                workers: Arc::new(Semaphore::new(limits.workers)),
                worker_limit: limits.workers,
                background: Arc::new(Semaphore::new(limits.background)),
                background_during_activity: Arc::new(Semaphore::new(BACKGROUND_DURING_ACTIVITY)),
                activity: std::sync::Mutex::new(ActivitySnapshot::default()),
                activity_changed: watch::channel(0).0,
                heavy: Arc::new(Semaphore::new(limits.heavy)),
                scratch: Arc::new(Semaphore::new(scratch_units)),
                scratch_limit_bytes: limits.scratch_bytes,
                active: std::array::from_fn(|_| AtomicUsize::new(0)),
                running: std::array::from_fn(|_| AtomicUsize::new(0)),
            }),
        }
    }

    /// The host activities currently recorded.
    pub fn activity(&self) -> ActivitySnapshot {
        *self
            .inner
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Notified with the ledger revision after every change.
    pub fn subscribe_activity(&self) -> watch::Receiver<u64> {
        self.inner.activity_changed.subscribe()
    }

    /// Records one host activity, sized from the ledger it joins: `size` sees
    /// every other activity's reservation and returns this one's, so two
    /// concurrent starts can never both size against the same free memory.
    /// A reservation the ledger cannot represent exactly, or a revision space
    /// that could not also order every live lease's release, is refused as
    /// `at_capacity`; revisions never repeat.
    pub fn begin_activity<E>(
        &self,
        size: impl FnOnce(&ActivitySnapshot) -> Result<u64, E>,
        at_capacity: E,
    ) -> Result<ActivityLease, E> {
        let mut ledger = self
            .inner
            .activity
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if ledger.count >= MAX_ACTIVITIES {
            return Err(at_capacity);
        }
        let reserved_mib = size(&ledger)?;
        let Some((total_mib, revision)) = ledger.reserved_mib.checked_add(reserved_mib).zip(
            // This admission, then one release per live lease including this one.
            ledger
                .revision
                .checked_add(ledger.count as u64 + 2)
                .map(|_| ledger.revision + 1),
        ) else {
            return Err(at_capacity);
        };
        ledger.count += 1;
        ledger.reserved_mib = total_mib;
        ledger.revision = revision;
        let revision = ledger.revision;
        drop(ledger);
        self.inner.activity_changed.send_replace(revision);
        Ok(ActivityLease {
            owner: Arc::clone(&self.inner),
            reserved_mib,
        })
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
        let scratch = self.reserve_scratch_inner(request.scratch_bytes).await?;
        let background = if request.class == PhysicalWorkClass::Background {
            Some(acquire_one(&self.inner.background).await?)
        } else {
            None
        };
        // While a game runs, background work is narrowed, never stopped.
        let quieted = if request.class == PhysicalWorkClass::Background
            && self.activity().count > 0
        {
            Some(acquire_one(&self.inner.background_during_activity).await?)
        } else {
            None
        };
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
            _background: background,
            _quieted: quieted,
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

    /// Reserve without waiting while earlier allocations remain retained.
    pub fn try_reserve_scratch(
        &self,
        bytes: u64,
    ) -> Result<Option<PhysicalScratchPermit>, PhysicalWorkError> {
        Ok(self
            .try_reserve_scratch_inner(bytes)?
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

    #[cfg(feature = "test-support")]
    pub fn scratch_limit_bytes(&self) -> u64 {
        self.inner.scratch_limit_bytes
    }

    /// Runs `work` on the calling thread with the admitted workers available
    /// to its scoped jobs.
    pub fn try_run_inline_parallel<T, Work>(
        &self,
        request: PhysicalWorkRequest,
        work: Work,
    ) -> Result<T, PhysicalWorkError>
    where
        Work: FnOnce(&PhysicalWorkParallelism) -> T,
    {
        Ok(self.try_admit(request)?.run_inline(work))
    }

    fn try_admit(
        &self,
        request: PhysicalWorkRequest,
    ) -> Result<PhysicalWorkAdmission, PhysicalWorkError> {
        validate_parallelism(&self.inner, request.parallelism)?;
        let scratch = self.try_reserve_scratch_inner(request.scratch_bytes)?;
        let background = if request.class == PhysicalWorkClass::Background {
            Some(try_acquire_one(&self.inner.background)?)
        } else {
            None
        };
        let quieted = if request.class == PhysicalWorkClass::Background
            && self.activity().count > 0
        {
            Some(try_acquire_one(&self.inner.background_during_activity)?)
        } else {
            None
        };
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
            _background: background,
            _quieted: quieted,
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
    /// Runs `work` on the calling thread with this admission's workers lent
    /// to its scoped jobs; the admission is released when `work` returns.
    pub fn run_inline<T, Work>(self, work: Work) -> T
    where
        Work: FnOnce(&PhysicalWorkParallelism) -> T,
    {
        let _admission = self;
        let _running = RunningWorker::new(&_admission);
        let parallelism = _admission.parallelism();
        work(&parallelism)
    }

    fn parallelism(&self) -> PhysicalWorkParallelism {
        PhysicalWorkParallelism {
            owner: Arc::clone(&self.owner),
            class: self.class,
            workers: self.parallelism,
            _not_send: std::marker::PhantomData,
        }
    }

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
        };
        let task = tokio::task::spawn_blocking(move || {
            let _admission = self;
            let _running = RunningWorker::new(&_admission);
            let parallelism = _admission.parallelism();
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

    /// Runs the first job on the calling thread and every other job on its
    /// own lent worker, returning the results in job order.
    pub fn run_scoped<T, Work>(&self, jobs: Vec<Work>) -> Result<Vec<T>, PhysicalWorkError>
    where
        T: Send,
        Work: FnOnce() -> T + Send,
    {
        if jobs.len() > self.workers {
            return Err(PhysicalWorkError::WorkerLimit);
        }
        run_jobs(jobs)
    }

    /// Runs read-only `job` over contiguous ranges covering `0..len` and
    /// returns the results in range order. This admission's workers are lent
    /// first; foreground work may also take idle process workers without
    /// waiting, while background work never grows beyond its admission. With
    /// one worker, or when a lent worker stops, the whole pass runs on the
    /// calling thread, so a job may run again and must not apply effects.
    pub fn run_ranges<T, Job>(&self, len: usize, max_workers: usize, job: Job) -> Vec<T>
    where
        T: Send,
        Job: Fn(std::ops::Range<usize>) -> T + Sync,
    {
        let wanted = len.min(max_workers).min(self.owner.worker_limit);
        if wanted <= 1 {
            return vec![job(0..len)];
        }
        let lent = self.workers.min(wanted);
        let extra = if self.class == PhysicalWorkClass::Foreground {
            (1..=wanted - lent).rev().find_map(|count| {
                let permit = try_acquire_many(&self.owner.workers, count).ok()?;
                Some((RunningWorker::extra(&self.owner, self.class, count), permit))
            })
        } else {
            None
        };
        let workers = lent + extra.as_ref().map_or(0, |(running, _)| running.count);
        if workers <= 1 {
            return vec![job(0..len)];
        }
        let span = len.div_ceil(workers);
        let job = &job;
        let jobs = (0..len.div_ceil(span))
            .map(|index| move || job(index * span..((index + 1) * span).min(len)))
            .collect();
        match run_jobs(jobs) {
            Ok(results) => results,
            Err(_) => {
                drop(extra);
                vec![job(0..len)]
            }
        }
    }
}

/// Spawns every job after the first, runs the first on the calling thread
/// and joins every spawned worker before returning or resuming a panic.
fn run_jobs<T, Work>(jobs: Vec<Work>) -> Result<Vec<T>, PhysicalWorkError>
where
    T: Send,
    Work: FnOnce() -> T + Send,
{
    let count = jobs.len();
    let mut jobs = jobs.into_iter();
    let Some(first) = jobs.next() else {
        return Ok(Vec::new());
    };
    std::thread::scope(|scope| {
        let mut failed = false;
        let mut workers = Vec::with_capacity(count - 1);
        for (index, job) in (1..).zip(jobs) {
            match std::thread::Builder::new()
                .name(format!("axial-physical-{index}"))
                .spawn_scoped(scope, job)
            {
                Ok(worker) => workers.push(worker),
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }
        let first =
            (!failed).then(|| std::panic::catch_unwind(std::panic::AssertUnwindSafe(first)));
        let joined = workers
            .into_iter()
            .map(std::thread::ScopedJoinHandle::join)
            .collect::<Vec<_>>();
        let mut results = Vec::with_capacity(count);
        match first {
            Some(Ok(result)) => results.push(result),
            Some(Err(panic)) => std::panic::resume_unwind(panic),
            None => {}
        }
        for result in joined {
            match result {
                Ok(result) => results.push(result),
                Err(_) => failed = true,
            }
        }
        if failed {
            Err(PhysicalWorkError::TaskStopped)
        } else {
            Ok(results)
        }
    })
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
        let registration = self.register()?;
        let mut cancelled = self.inner.cancellation.subscribe();
        let admission = tokio::select! {
            admission = self.owner.admit(request) => admission?,
            changed = cancelled.changed() => {
                let _ = changed;
                return Err(PhysicalWorkError::Cancelled);
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
        let result = map_task_result(task.await);
        cancel_on_drop.armed = false;
        result
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
        Self::extra(&admission.owner, admission.class, admission.parallelism)
    }

    /// Workers that run for an existing admission without adding another.
    fn extra(owner: &Arc<PhysicalWorkOwnerInner>, class: PhysicalWorkClass, count: usize) -> Self {
        owner.running[class.index()].fetch_add(count, Ordering::AcqRel);
        Self {
            owner: Arc::clone(owner),
            class,
            count,
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
    heavy: usize,
    scratch_bytes: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_leases_size_against_each_other_and_release_exactly() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let first = owner
            .begin_activity(|ledger| Ok::<_, ()>(4096 - ledger.reserved_mib.min(4096)), ())
            .unwrap();
        let second = owner
            .begin_activity(
                |ledger| Ok::<_, ()>(4096u64.saturating_sub(ledger.reserved_mib).max(512)),
                (),
            )
            .unwrap();
        assert_eq!((first.reserved_mib(), second.reserved_mib()), (4096, 512));
        let snapshot = owner.activity();
        assert_eq!((snapshot.count, snapshot.reserved_mib), (2, 4608));
        drop(first);
        let after = owner.activity();
        assert_eq!((after.count, after.reserved_mib), (1, 512));
        assert!(after.revision > snapshot.revision);
        assert_eq!(owner.begin_activity(|_| Err("refused"), "full").unwrap_err(), "refused");
        // A reservation the ledger cannot add exactly is refused, not saturated.
        assert_eq!(
            owner.begin_activity(|_| Ok(u64::MAX), "full").unwrap_err(),
            "full"
        );
        assert_eq!(owner.activity(), after);
        drop(second);
        assert_eq!(owner.activity().count, 0);

        // Revisions are refused before they could repeat, keeping one for
        // every live lease's release.
        let exhausted = PhysicalWorkOwner::new(PROCESS_LIMITS);
        exhausted.inner.activity.lock().unwrap().revision = u64::MAX - 3;
        let live = exhausted.begin_activity(|_| Ok::<_, ()>(1), ()).unwrap();
        assert_eq!(exhausted.activity().revision, u64::MAX - 2);
        assert_eq!(exhausted.begin_activity(|_| Ok(1), "full").unwrap_err(), "full");
        drop(live);
        assert_eq!(exhausted.activity().revision, u64::MAX - 1);
        assert_eq!(exhausted.activity().count, 0);
    }

    #[tokio::test]
    async fn a_running_activity_narrows_background_work_but_never_foreground() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let lease = owner.begin_activity(|_| Ok::<_, ()>(1024), ()).unwrap();
        let first = owner
            .admit(PhysicalWorkRequest::background(PhysicalIoClass::Read, 0))
            .await
            .unwrap();
        assert!(matches!(
            owner.try_admit(PhysicalWorkRequest::background(PhysicalIoClass::Read, 0)),
            Err(PhysicalWorkError::Unavailable)
        ));
        let foreground = owner
            .try_admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0))
            .unwrap();
        drop(lease);
        let second = owner
            .try_admit(PhysicalWorkRequest::background(PhysicalIoClass::Read, 0))
            .unwrap();
        drop((first, second, foreground));
    }
    use std::sync::{Condvar, Mutex};
    use std::time::Duration;

    fn test_owner() -> PhysicalWorkOwner {
        PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 3,
            background: 2,
            heavy: 1,
            scratch_bytes: 2 * SCRATCH_UNIT_BYTES,
        })
    }

    type Gate = Arc<(Mutex<bool>, Condvar)>;

    fn wait(gate: &Gate) {
        let (lock, wake) = &**gate;
        drop(wake.wait_while(lock.lock().unwrap(), |open| !*open).unwrap());
    }

    fn open(gate: &Gate) {
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
    }

    #[test]
    fn inline_parallel_work_lends_every_admitted_worker() {
        let owner = test_owner();
        let observed_owner = owner.clone();
        let results = owner
            .try_run_inline_parallel(
                PhysicalWorkRequest::foreground_parallel(PhysicalIoClass::Read, 0, 3),
                move |parallelism| {
                    assert_eq!(parallelism.workers(), 3);
                    let snapshot = observed_owner.snapshot(PhysicalWorkClass::Foreground);
                    assert_eq!(snapshot.running_workers, 3);
                    assert_eq!(
                        observed_owner.try_run_inline_parallel(
                            PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                            |_| (),
                        ),
                        Err(PhysicalWorkError::Unavailable)
                    );
                    parallelism.run_scoped((0..3).map(|index| move || index).collect())
                },
            )
            .expect("inline parallel admission")
            .expect("scoped jobs");
        assert_eq!(results, vec![0, 1, 2]);
        let snapshot = owner.snapshot(PhysicalWorkClass::Foreground);
        assert_eq!(snapshot.active_admissions, 0);
        assert_eq!(snapshot.running_workers, 0);
        assert_eq!(snapshot.available_workers, 3);
    }

    fn idle(owner: &PhysicalWorkOwner) -> [PhysicalWorkSnapshot; 2] {
        [PhysicalWorkClass::Foreground, PhysicalWorkClass::Background]
            .map(|class| owner.snapshot(class))
    }

    #[test]
    #[allow(clippy::single_range_in_vec_init)]
    fn ranges_lend_the_caller_then_take_only_idle_foreground_workers() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let before = idle(&owner);
        let caller = std::thread::current().id();
        for (holding, width, ranges) in [
            (0, 4, vec![0..3, 3..6, 6..9, 9..10]),
            (1, 3, vec![0..4, 4..8, 8..10]),
            (2, 2, vec![0..5, 5..10]),
            (3, 1, vec![0..10]),
        ] {
            let held = (holding > 0).then(|| {
                owner
                    .try_admit(PhysicalWorkRequest::foreground_parallel(
                        PhysicalIoClass::Read,
                        0,
                        holding,
                    ))
                    .unwrap()
            });
            let observed = owner
                .try_run_inline_parallel(
                    PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                    |parallelism| {
                        let gate = std::sync::Barrier::new(width);
                        parallelism.run_ranges(10, 4, |range| {
                            let snapshot = owner.snapshot(PhysicalWorkClass::Foreground);
                            gate.wait();
                            (range, std::thread::current().id(), snapshot)
                        })
                    },
                )
                .unwrap();
            drop(held);
            assert_eq!(
                observed
                    .iter()
                    .map(|(range, ..)| range.clone())
                    .collect::<Vec<_>>(),
                ranges
            );
            assert_eq!(observed[0].1, caller, "the caller runs the first range");
            let threads = observed
                .iter()
                .map(|(_, thread, _)| *thread)
                .collect::<std::collections::HashSet<_>>();
            assert_eq!(threads.len(), width);
            for (_, _, snapshot) in &observed {
                // Extra workers run for the one admission without adding another.
                assert_eq!(
                    (
                        snapshot.active_admissions,
                        snapshot.running_workers,
                        snapshot.available_workers
                    ),
                    (1 + usize::from(holding > 0), width, 0)
                );
            }
            assert_eq!(idle(&owner), before);
        }

        for (len, max_workers, ranges) in [
            (5, 4, vec![0..2, 2..4, 4..5]),
            (0, 4, vec![0..0]),
            (1, 4, vec![0..1]),
            (10, 1, vec![0..10]),
        ] {
            let observed = owner
                .try_run_inline_parallel(
                    PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                    |parallelism| parallelism.run_ranges(len, max_workers, |range| range),
                )
                .unwrap();
            assert_eq!(observed, ranges, "{len} entries on {max_workers} workers");
            assert_eq!(idle(&owner), before);
        }
    }

    #[test]
    fn background_ranges_never_grow_beyond_their_admission() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let before = idle(&owner);
        let caller = std::thread::current().id();
        let observed = owner
            .try_run_inline_parallel(
                PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                |parallelism| {
                    parallelism.run_ranges(10, 4, |range| {
                        (
                            range,
                            std::thread::current().id(),
                            owner.snapshot(PhysicalWorkClass::Background),
                        )
                    })
                },
            )
            .unwrap();
        assert_eq!(observed.len(), 1);
        let (range, thread, snapshot) = &observed[0];
        assert_eq!((range.clone(), *thread), (0..10, caller));
        assert_eq!(
            (
                snapshot.active_admissions,
                snapshot.running_workers,
                snapshot.available_workers
            ),
            (1, 1, PROCESS_WORKER_LIMIT - 1)
        );
        assert_eq!(idle(&owner), before);
    }

    #[test]
    fn a_stopped_range_worker_is_joined_then_the_pass_repeats_inline() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let before = idle(&owner);
        let caller = std::thread::current().id();
        let stopped = AtomicBool::new(false);
        let completed = AtomicUsize::new(0);
        let observed = owner
            .try_run_inline_parallel(
                PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                |parallelism| {
                    parallelism.run_ranges(8, 4, |range| {
                        if range.start == 2 && !stopped.swap(true, Ordering::AcqRel) {
                            panic!("fixture worker stops once");
                        }
                        completed.fetch_add(1, Ordering::AcqRel);
                        (range, std::thread::current().id())
                    })
                },
            )
            .unwrap();
        // Three other ranges completed before the inline repetition.
        assert_eq!(completed.load(Ordering::Acquire), 4);
        assert_eq!(observed, vec![(0..8, caller)]);
        assert_eq!(idle(&owner), before);
    }

    #[test]
    fn a_caller_range_panic_resumes_only_after_lent_workers_join() {
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let before = idle(&owner);
        let gate: Gate = Arc::default();
        let joined = AtomicUsize::new(0);
        let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.try_run_inline_parallel(
                PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                |parallelism| {
                    parallelism.run_ranges(4, 4, |range| {
                        if range.start == 0 {
                            open(&gate);
                            panic!("fixture caller range stops");
                        }
                        wait(&gate);
                        joined.fetch_add(1, Ordering::AcqRel);
                    })
                },
            )
        }));
        assert!(panicked.is_err());
        assert_eq!(joined.load(Ordering::Acquire), 3);
        assert_eq!(idle(&owner), before);
    }

    #[test]
    fn inline_admission_runs_on_the_calling_thread_and_releases_on_return() {
        let owner = test_owner();
        let before = idle(&owner);
        let caller = std::thread::current().id();
        let admission = owner
            .try_admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Read,
                SCRATCH_UNIT_BYTES,
                2,
            ))
            .unwrap();
        let (thread, workers, snapshot) = admission.run_inline(|parallelism| {
            (
                std::thread::current().id(),
                parallelism.workers(),
                owner.snapshot(PhysicalWorkClass::Foreground),
            )
        });
        assert_eq!((thread, workers), (caller, 2));
        assert_eq!(
            (
                snapshot.active_admissions,
                snapshot.running_workers,
                snapshot.available_workers,
                snapshot.available_scratch_bytes
            ),
            (1, 2, 1, SCRATCH_UNIT_BYTES)
        );
        assert_eq!(idle(&owner), before);
    }

    #[tokio::test]
    async fn running_counts_are_settled_before_released_workers_wake_waiters() {
        use std::{
            future::Future,
            task::{Context, Wake, Waker},
        };

        struct ObserveWake {
            owner: PhysicalWorkOwner,
            running: Mutex<Vec<usize>>,
        }

        impl Wake for ObserveWake {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                let running = self.owner.snapshot(PhysicalWorkClass::Foreground).running_workers;
                self.running.lock().unwrap().push(running);
            }
        }

        let owner = test_owner();
        let before = idle(&owner);
        for inline in [true, false] {
            let observed = Arc::new(ObserveWake {
                owner: owner.clone(),
                running: Mutex::new(Vec::new()),
            });
            let pending = {
                let admission = owner
                    .try_admit(PhysicalWorkRequest::foreground_parallel(
                        PhysicalIoClass::Read, 0, 3,
                    ))
                    .unwrap();
                let waiting = owner.admit(PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0));
                let mut waiting = std::pin::pin!(waiting);
                let waker = Waker::from(Arc::clone(&observed));
                let pending = waiting.as_mut().poll(&mut Context::from_waker(&waker)).is_pending();
                if inline {
                    admission.run_inline(|_| ());
                } else {
                    admission.run_parallel(|_, _| ()).await.unwrap();
                }
                pending
            };
            let running = observed.running.lock().unwrap().clone();
            assert!(pending);
            assert!(!running.is_empty());
            assert!(running.iter().all(|count| *count == 0), "{running:?}");
            assert_eq!(idle(&owner), before);
        }
    }

    #[tokio::test]
    async fn dropped_and_cancelled_waiting_admissions_restore_capacity() {
        use std::future::Future;
        let owner = test_owner();
        let before = idle(&owner);
        let held = owner
            .try_admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Read,
                2 * SCRATCH_UNIT_BYTES,
                3,
            ))
            .unwrap();
        for request in [
            PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
            PhysicalWorkRequest::foreground(PhysicalIoClass::Heavy, SCRATCH_UNIT_BYTES),
        ] {
            let waiting = owner.admit(request);
            tokio::pin!(waiting);
            let pending = std::future::poll_fn(|context| {
                std::task::Poll::Ready(waiting.as_mut().poll(context).is_pending())
            })
            .await;
            assert!(pending, "exhausted capacity keeps the admission waiting");
        }
        let group = owner.group();
        let queued = tokio::spawn({
            let group = group.clone();
            async move {
                group
                    .run(
                        PhysicalWorkRequest::foreground(PhysicalIoClass::Read, 0),
                        |_| (),
                    )
                    .await
            }
        });
        while group.active() == 0 {
            tokio::task::yield_now().await;
        }
        group.cancel();
        assert_eq!(queued.await.unwrap(), Err(PhysicalWorkError::Cancelled));
        group.drain().await;
        drop(held);
        assert_eq!(idle(&owner), before);
        owner
            .admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Heavy,
                2 * SCRATCH_UNIT_BYTES,
                3,
            ))
            .await
            .unwrap()
            .run_inline(|parallelism| assert_eq!(parallelism.workers(), 3));
        assert_eq!(idle(&owner), before);
    }

    #[tokio::test]
    async fn parallel_admission_is_bounded_and_reserves_every_worker() {
        let owner = test_owner();
        for parallelism in [0, 4] {
            let request =
                PhysicalWorkRequest::foreground_parallel(PhysicalIoClass::Metadata, 0, parallelism);
            assert!(matches!(
                owner.admit(request).await,
                Err(PhysicalWorkError::WorkerLimit)
            ));
        }
        let admission = owner
            .admit(PhysicalWorkRequest::foreground_parallel(
                PhysicalIoClass::Metadata,
                0,
                3,
            ))
            .await
            .expect("parallel admission");
        let observed_owner = owner.clone();
        let results = admission
            .run_parallel(move |_, parallelism| {
                let snapshot = observed_owner.snapshot(PhysicalWorkClass::Foreground);
                assert_eq!((snapshot.running_workers, snapshot.available_workers), (3, 0));
                parallelism.run_scoped((0..3).map(|index| move || index).collect())
            })
            .await
            .expect("parallel worker")
            .expect("scoped jobs");
        assert_eq!(results, vec![0, 1, 2]);
        let snapshot = owner.snapshot(PhysicalWorkClass::Foreground);
        assert_eq!(snapshot.active_admissions, 0);
        assert_eq!(snapshot.available_workers, 3);
    }

    #[tokio::test]
    async fn scratch_waiter_does_not_hold_background_capacity() {
        let owner = PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 2,
            background: 1,
            heavy: 1,
            scratch_bytes: SCRATCH_UNIT_BYTES,
        });
        let scratch = owner
            .reserve_scratch(SCRATCH_UNIT_BYTES)
            .await
            .expect("reserve process scratch")
            .expect("nonzero scratch permit");
        let waiting = owner.group();
        let waiting_task = tokio::spawn({
            let waiting = waiting.clone();
            async move {
                waiting
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, SCRATCH_UNIT_BYTES),
                        |_| (),
                    )
                    .await
            }
        });
        while waiting.active() == 0 {
            tokio::task::yield_now().await;
        }
        tokio::time::timeout(
            Duration::from_secs(1),
            owner.group().run(
                PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                |_| (),
            ),
        )
        .await
        .expect("scratch waiter must not occupy background capacity")
        .expect("independent background work");

        drop(scratch);
        waiting_task
            .await
            .expect("join scratch waiter")
            .expect("scratch waiter");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn queued_background_work_leaves_foreground_admission_and_cancels() {
        let owner = PhysicalWorkOwner::new(PhysicalWorkLimits {
            workers: 2,
            background: 1,
            heavy: 1,
            scratch_bytes: 2 * SCRATCH_UNIT_BYTES,
        });
        let first = owner.group();
        let second = owner.group();
        let gate: Gate = Arc::default();
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let first_task = tokio::spawn({
            let (first, gate) = (first.clone(), Arc::clone(&gate));
            async move {
                first
                    .run(
                        PhysicalWorkRequest::background(PhysicalIoClass::Read, 0),
                        move |_| {
                            let _ = started_tx.send(());
                            wait(&gate);
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
        open(&gate);
        assert_eq!(first_task.await.expect("join first work"), Ok(()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn saturated_process_limits_queue_cancel_drain_and_restore_capacity() {
        const WORKER_SCRATCH_BYTES: u64 = SCRATCH_LIMIT_BYTES / PROCESS_WORKER_LIMIT as u64;
        let owner = PhysicalWorkOwner::new(PROCESS_LIMITS);
        let gate: Gate = Arc::default();
        let (started_tx, mut started_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut running = Vec::with_capacity(PROCESS_WORKER_LIMIT);
        for index in 0..PROCESS_WORKER_LIMIT {
            let group = owner.group();
            let (gate, started_tx) = (Arc::clone(&gate), started_tx.clone());
            let request = if index < BACKGROUND_WORKER_LIMIT {
                PhysicalWorkRequest::background(PhysicalIoClass::Read, WORKER_SCRATCH_BYTES)
            } else {
                PhysicalWorkRequest::foreground(PhysicalIoClass::Read, WORKER_SCRATCH_BYTES)
            };
            let task = tokio::spawn({
                let group = group.clone();
                async move {
                    group
                        .run(request, move |cancellation| {
                            started_tx.send(()).unwrap();
                            wait(&gate);
                            cancellation.is_cancelled()
                        })
                        .await
                }
            });
            running.push((group, task));
        }
        drop(started_tx);
        for _ in 0..PROCESS_WORKER_LIMIT {
            tokio::time::timeout(Duration::from_secs(2), started_rx.recv())
                .await
                .expect("saturated worker starts")
                .expect("saturated worker signal remains open");
        }
        let foreground = owner.snapshot(PhysicalWorkClass::Foreground);
        let background = owner.snapshot(PhysicalWorkClass::Background);
        assert_eq!(
            foreground.running_workers + background.running_workers,
            PROCESS_WORKER_LIMIT
        );
        assert_eq!(foreground.available_workers, 0);
        assert_eq!(foreground.available_scratch_bytes, 0);

        let queued = owner.group();
        let (queued_tx, mut queued_rx) = tokio::sync::oneshot::channel();
        let queued_task = tokio::spawn({
            let queued = queued.clone();
            async move {
                queued
                    .run(
                        PhysicalWorkRequest::foreground(PhysicalIoClass::Metadata, 0),
                        move |_| queued_tx.send(()).unwrap(),
                    )
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(20), &mut queued_rx)
                .await
                .is_err(),
            "saturated work must keep the next worker queued"
        );

        for (group, _) in &running {
            group.cancel();
        }
        open(&gate);
        for (group, task) in running {
            assert_eq!(task.await.expect("join saturated task"), Ok(true));
            group.drain().await;
        }
        assert_eq!(queued_task.await.expect("join queued task"), Ok(()));
        queued.drain().await;
        for class in [PhysicalWorkClass::Foreground, PhysicalWorkClass::Background] {
            let snapshot = owner.snapshot(class);
            assert_eq!(snapshot.active_admissions, 0);
            assert_eq!(snapshot.running_workers, 0);
            assert_eq!(snapshot.available_workers, PROCESS_WORKER_LIMIT);
            assert_eq!(snapshot.available_scratch_bytes, SCRATCH_LIMIT_BYTES);
        }
    }
}
