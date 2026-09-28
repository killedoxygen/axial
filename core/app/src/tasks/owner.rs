use super::CancellationToken;
use std::{
    collections::BTreeMap,
    fmt,
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use tokio::sync::{oneshot, watch};

static NEXT_TASK_ID: AtomicU64 = AtomicU64::new(1);

/// An in-process incarnation identity. It is not a durable domain operation ID.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct TaskId(u64);

impl fmt::Display for TaskId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum SpawnError {
    InvalidCapacity,
    AtCapacity,
    Closed,
    NoRuntime,
    IdentityExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskJoinError {
    Panicked,
    Interrupted,
    OwnerLost,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerSnapshot {
    pub closing: bool,
    pub running: Vec<TaskId>,
    /// A panic or runtime interruption cannot prove domain settlement.
    pub unsettled: Vec<TaskId>,
}

impl OwnerSnapshot {
    pub fn is_idle(&self) -> bool {
        self.running.is_empty() && self.unsettled.is_empty()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct OwnerBusy {
    pub work: OwnerSnapshot,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ShutdownError {
    pub work: OwnerSnapshot,
}

macro_rules! debug_error {
    ($($name:ty),+ $(,)?) => {
        $(impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, "{self:?}")
            }
        }
        impl std::error::Error for $name {})+
    };
}

debug_error!(SpawnError, TaskJoinError, OwnerBusy, ShutdownError);

struct Record {
    cancellation: CancellationToken,
    unsettled: bool,
    // Held outside the user's future: even unwinding that future cannot revoke
    // its library generation or exclusion while settlement is unknown.
    retained: Option<Box<dyn Send>>,
}

struct State {
    closing: bool,
    tasks: BTreeMap<TaskId, Record>,
}

impl State {
    fn snapshot(&self) -> OwnerSnapshot {
        let (unsettled, running) = self
            .tasks
            .iter()
            .partition::<Vec<_>, _>(|(_, record)| record.unsettled);
        OwnerSnapshot {
            closing: self.closing,
            running: running.into_iter().map(|(id, _)| *id).collect(),
            unsettled: unsettled.into_iter().map(|(id, _)| *id).collect(),
        }
    }
}

struct Inner {
    capacity: usize,
    state: Mutex<State>,
    changed: watch::Sender<()>,
}

// Constructed before spawning so even a runtime that never polls the supervisor
// cannot discard accepted work without reporting its unknown settlement.
struct Supervisor<T> {
    inner: Arc<Inner>,
    id: TaskId,
    result: Option<oneshot::Sender<Result<T, TaskJoinError>>>,
}

impl<T> Supervisor<T> {
    fn complete(mut self, outcome: Result<T, TaskJoinError>) {
        if let Some(sender) = self.result.take() {
            let _ = sender.send(outcome);
        }
    }
}

impl<T> Drop for Supervisor<T> {
    fn drop(&mut self) {
        let Some(sender) = self.result.take() else {
            return;
        };
        if let Some(record) = self
            .inner
            .state
            .lock()
            .expect("task owner lock poisoned")
            .tasks
            .get_mut(&self.id)
        {
            record.unsettled = true;
        }
        self.inner.changed.send_replace(());
        let _ = sender.send(Err(TaskJoinError::Interrupted));
    }
}

/// Bounded owner of accepted work. There is no unbounded internal queue.
#[derive(Clone)]
pub struct TaskOwner {
    inner: Arc<Inner>,
}

/// Proof that one exact task owner is permanently closed and fully joined.
/// Reset composition must check this against its authoritative shared owner.
#[derive(Clone)]
pub struct ShutdownReceipt {
    owner: Arc<Inner>,
}

impl ShutdownReceipt {
    pub fn belongs_to(&self, owner: &TaskOwner) -> bool {
        Arc::ptr_eq(&self.owner, &owner.inner)
    }
}

impl fmt::Debug for ShutdownReceipt {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ShutdownReceipt")
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for TaskOwner {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TaskOwner")
            .field("work", &self.status())
            .finish()
    }
}

impl TaskOwner {
    pub fn new(max_active: usize) -> Result<Self, SpawnError> {
        if max_active == 0 {
            return Err(SpawnError::InvalidCapacity);
        }
        Ok(Self {
            inner: Arc::new(Inner {
                capacity: max_active,
                state: Mutex::new(State {
                    closing: false,
                    tasks: BTreeMap::new(),
                }),
                changed: watch::channel(()).0,
            }),
        })
    }

    /// Acceptance is synchronous and atomic with admission closure. `retained`
    /// normally contains the library pin and complete exclusion lease. The
    /// future must not spawn detached effects that outlive these guards.
    pub fn try_spawn<G, F, Fut, T>(&self, retained: G, work: F) -> Result<TaskHandle<T>, SpawnError>
    where
        G: Send + 'static,
        F: FnOnce(CancellationToken) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| SpawnError::NoRuntime)?;
        let cancellation = CancellationToken::new();
        let (sender, result) = oneshot::channel();
        let id;
        {
            let mut state = self.inner.state.lock().expect("task owner lock poisoned");
            if state.closing {
                return Err(SpawnError::Closed);
            }
            if state.tasks.len() >= self.inner.capacity {
                return Err(SpawnError::AtCapacity);
            }
            id = TaskId(
                NEXT_TASK_ID
                    .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
                    .map_err(|_| SpawnError::IdentityExhausted)?,
            );
            state.tasks.insert(
                id,
                Record {
                    cancellation: cancellation.clone(),
                    unsettled: false,
                    retained: Some(Box::new(retained)),
                },
            );
        }
        self.inner.changed.send_replace(());

        let inner = Arc::clone(&self.inner);
        let worker_cancellation = cancellation.clone();
        let supervisor = Supervisor {
            inner: Arc::clone(&inner),
            id,
            result: Some(sender),
        };
        // The supervisor keeps ownership after every request/handle is dropped.
        // The inner join catches panics, including panics constructing `work`.
        runtime.spawn(async move {
            let outcome = tokio::spawn(async move { work(worker_cancellation).await }).await;
            let outcome = match outcome {
                Ok(value) => {
                    let retained = inner
                        .state
                        .lock()
                        .expect("task owner lock poisoned")
                        .tasks
                        .get_mut(&id)
                        .and_then(|record| record.retained.take());
                    // Do not report idle until resource destructors complete.
                    // Resource release itself runs outside the memory mutex.
                    drop(retained);
                    inner
                        .state
                        .lock()
                        .expect("task owner lock poisoned")
                        .tasks
                        .remove(&id);
                    Ok(value)
                }
                Err(error) => {
                    if let Some(record) = inner
                        .state
                        .lock()
                        .expect("task owner lock poisoned")
                        .tasks
                        .get_mut(&id)
                    {
                        record.unsettled = true;
                    }
                    Err(if error.is_panic() {
                        TaskJoinError::Panicked
                    } else {
                        TaskJoinError::Interrupted
                    })
                }
            };
            inner.changed.send_replace(());
            supervisor.complete(outcome);
        });

        Ok(TaskHandle {
            id,
            cancellation,
            result,
        })
    }

    pub fn status(&self) -> OwnerSnapshot {
        self.inner
            .state
            .lock()
            .expect("task owner lock poisoned")
            .snapshot()
    }

    /// Subscribe before checking admission or status to avoid a missed wakeup.
    /// Notifications are coalesced hints; retry the authoritative admission
    /// operation after a change, since another consumer may take the free slot.
    pub fn subscribe(&self) -> watch::Receiver<()> {
        self.inner.changed.subscribe()
    }

    pub fn shutdown_receipt(&self) -> Option<ShutdownReceipt> {
        let state = self.inner.state.lock().expect("task owner lock poisoned");
        (state.closing && state.tasks.is_empty()).then(|| ShutdownReceipt {
            owner: Arc::clone(&self.inner),
        })
    }

    pub fn cancel(&self, id: TaskId) -> bool {
        let token = self
            .inner
            .state
            .lock()
            .expect("task owner lock poisoned")
            .tasks
            .get(&id)
            .map(|record| record.cancellation.clone());
        token.is_some_and(|token| token.cancel())
    }

    /// Atomic busy refusal for close/restart/update. Failure leaves admission
    /// open; success closes it permanently. This is not a racy idle query.
    pub fn try_close_idle(&self) -> Result<(), OwnerBusy> {
        let mut state = self.inner.state.lock().expect("task owner lock poisoned");
        if !state.tasks.is_empty() {
            return Err(OwnerBusy {
                work: state.snapshot(),
            });
        }
        state.closing = true;
        self.inner.changed.send_replace(());
        Ok(())
    }

    /// Reset closes admission before its retained shutdown worker requests
    /// cancellation and joins accepted work. Existing guards remain owned.
    pub fn close_admission(&self) {
        self.inner
            .state
            .lock()
            .expect("task owner lock poisoned")
            .closing = true;
        self.inner.changed.send_replace(());
    }

    /// Close admission, request cooperative cancellation and join all accepted
    /// work. Timeouts keep work and guards alive. Calling again safely retries.
    /// Panicked tasks stay visibly unsettled; generic code cannot clear them.
    pub async fn shutdown(&self, timeout: Duration) -> Result<(), ShutdownError> {
        let mut changes = self.inner.changed.subscribe();
        let tokens = {
            let mut state = self.inner.state.lock().expect("task owner lock poisoned");
            state.closing = true;
            state
                .tasks
                .values()
                .map(|record| record.cancellation.clone())
                .collect::<Vec<_>>()
        };
        for token in tokens {
            token.cancel();
        }
        let deadline = tokio::time::sleep(timeout);
        tokio::pin!(deadline);
        loop {
            let snapshot = self.status();
            if snapshot.is_idle() {
                return Ok(());
            }
            if snapshot.running.is_empty() {
                return Err(ShutdownError { work: snapshot });
            }
            tokio::select! {
                _ = &mut deadline => {
                    let snapshot = self.status();
                    return if snapshot.is_idle() {
                        Ok(())
                    } else {
                        Err(ShutdownError { work: snapshot })
                    };
                }
                changed = changes.changed() => {
                    if changed.is_err() {
                        return Err(ShutdownError { work: self.status() });
                    }
                }
            }
        }
    }
}

/// A disposable waiter. Explicit cancellation is separate from dropping it.
pub struct TaskHandle<T> {
    id: TaskId,
    cancellation: CancellationToken,
    result: oneshot::Receiver<Result<T, TaskJoinError>>,
}

impl<T> TaskHandle<T> {
    pub fn id(&self) -> TaskId {
        self.id
    }

    pub fn cancel(&self) -> bool {
        self.cancellation.cancel()
    }

    pub async fn join(self) -> Result<T, TaskJoinError> {
        self.result.await.unwrap_or(Err(TaskJoinError::OwnerLost))
    }
}

#[cfg(test)]
mod tests;
