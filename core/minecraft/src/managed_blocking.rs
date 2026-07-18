use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

#[derive(Clone)]
pub(crate) struct ManagedBlockingWorkers {
    inner: Arc<ManagedBlockingWorkersInner>,
}

pub(crate) struct ManagedBlockingCancellationGuard {
    workers: ManagedBlockingWorkers,
}

pub(crate) struct ManagedBlockingAttemptGuard {
    workers: ManagedBlockingWorkers,
    armed: bool,
}

#[derive(Clone)]
pub(crate) struct ManagedCancellation {
    inner: Arc<ManagedBlockingWorkersInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedBlockingTaskError {
    Cancelled,
    TaskStopped,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedBlockingCheckpoint {
    CacheHash,
    LibraryValidation,
    SourceSpool,
}

struct ManagedBlockingWorkersInner {
    cancelled: AtomicBool,
    state: Mutex<ManagedBlockingWorkersState>,
    active: watch::Sender<usize>,
    #[cfg(test)]
    checkpoint_hook: Option<Arc<dyn Fn(ManagedBlockingCheckpoint) + Send + Sync>>,
}

struct ManagedBlockingWorkersState {
    accepting: bool,
    active: usize,
}

struct ManagedBlockingWorkerRegistration {
    inner: Arc<ManagedBlockingWorkersInner>,
}

enum ManagedBlockingTaskOutput<T> {
    Cancelled,
    Complete(T),
}

impl ManagedBlockingWorkers {
    pub(crate) fn new() -> Self {
        Self {
            inner: Arc::new(ManagedBlockingWorkersInner {
                cancelled: AtomicBool::new(false),
                state: Mutex::new(ManagedBlockingWorkersState {
                    accepting: true,
                    active: 0,
                }),
                active: watch::channel(0).0,
                #[cfg(test)]
                checkpoint_hook: None,
            }),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_with_checkpoint_hook(
        hook: Arc<dyn Fn(ManagedBlockingCheckpoint) + Send + Sync>,
    ) -> Self {
        Self {
            inner: Arc::new(ManagedBlockingWorkersInner {
                cancelled: AtomicBool::new(false),
                state: Mutex::new(ManagedBlockingWorkersState {
                    accepting: true,
                    active: 0,
                }),
                active: watch::channel(0).0,
                checkpoint_hook: Some(hook),
            }),
        }
    }

    pub(crate) fn cancellation_guard(&self) -> ManagedBlockingCancellationGuard {
        ManagedBlockingCancellationGuard {
            workers: self.clone(),
        }
    }

    pub(crate) fn attempt_guard(&self) -> ManagedBlockingAttemptGuard {
        ManagedBlockingAttemptGuard {
            workers: self.clone(),
            armed: true,
        }
    }

    pub(crate) fn cancellation(&self) -> ManagedCancellation {
        ManagedCancellation {
            inner: Arc::clone(&self.inner),
        }
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn ensure_active(&self) -> Result<(), ManagedBlockingTaskError> {
        if self.is_cancelled() {
            Err(ManagedBlockingTaskError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn cancel(&self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.accepting = false;
        self.inner.cancelled.store(true, Ordering::Release);
        drop(state);
    }

    pub(crate) async fn run<T, F>(&self, work: F) -> Result<T, ManagedBlockingTaskError>
    where
        T: Send + 'static,
        F: FnOnce(ManagedCancellation) -> T + Send + 'static,
    {
        let registration = self.register()?;
        let cancellation = self.cancellation();
        let task = tokio::task::spawn_blocking(move || {
            let _registration = registration;
            if cancellation.is_cancelled() {
                ManagedBlockingTaskOutput::Cancelled
            } else {
                ManagedBlockingTaskOutput::Complete(work(cancellation))
            }
        });
        match task.await {
            Ok(ManagedBlockingTaskOutput::Complete(output)) => Ok(output),
            Ok(ManagedBlockingTaskOutput::Cancelled) => Err(ManagedBlockingTaskError::Cancelled),
            Err(_) => Err(ManagedBlockingTaskError::TaskStopped),
        }
    }

    pub(crate) async fn drain(&self) {
        let mut active = self.inner.active.subscribe();
        loop {
            if *active.borrow_and_update() == 0 {
                return;
            }
            active
                .changed()
                .await
                .expect("managed blocking worker counter remains owned");
        }
    }

    fn register(&self) -> Result<ManagedBlockingWorkerRegistration, ManagedBlockingTaskError> {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !state.accepting {
            return Err(ManagedBlockingTaskError::Cancelled);
        }
        state.active = state
            .active
            .checked_add(1)
            .expect("managed blocking worker count overflowed");
        self.inner.active.send_replace(state.active);
        Ok(ManagedBlockingWorkerRegistration {
            inner: Arc::clone(&self.inner),
        })
    }
}

impl ManagedCancellation {
    pub(crate) fn is_cancelled(&self) -> bool {
        self.inner.cancelled.load(Ordering::Acquire)
    }

    pub(crate) fn check_io(&self) -> io::Result<()> {
        if self.is_cancelled() {
            Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "managed blocking work was cancelled",
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn checkpoint(&self, checkpoint: ManagedBlockingCheckpoint) {
        #[cfg(not(test))]
        let _ = checkpoint;
        #[cfg(test)]
        if let Some(hook) = &self.inner.checkpoint_hook {
            hook(checkpoint);
        }
    }
}

impl Drop for ManagedBlockingCancellationGuard {
    fn drop(&mut self) {
        self.workers.cancel();
    }
}

impl ManagedBlockingAttemptGuard {
    pub(crate) fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for ManagedBlockingAttemptGuard {
    fn drop(&mut self) {
        if self.armed {
            self.workers.cancel();
        }
    }
}

impl Drop for ManagedBlockingWorkerRegistration {
    fn drop(&mut self) {
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state
            .active
            .checked_sub(1)
            .expect("managed blocking worker count underflowed");
        self.inner.active.send_replace(state.active);
        drop(state);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::sync::{Condvar, Mutex};

    #[tokio::test]
    async fn cancellation_closes_registration_before_zero_is_observed() {
        let workers = ManagedBlockingWorkers::new();
        workers.cancel();
        workers.drain().await;

        let starts = Arc::new(AtomicUsize::new(0));
        let starts_for_task = Arc::clone(&starts);
        assert_eq!(
            workers
                .run(move |_| {
                    starts_for_task.fetch_add(1, Ordering::Relaxed);
                })
                .await,
            Err(ManagedBlockingTaskError::Cancelled)
        );
        assert_eq!(starts.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn cancellation_drain_waits_for_registered_blocking_owner() {
        let workers = ManagedBlockingWorkers::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_task = Arc::clone(&gate);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let workers_for_task = workers.clone();
        let task = tokio::spawn(async move {
            workers_for_task
                .run(move |cancellation| {
                    let _ = entered_tx.send(());
                    let (lock, condition) = &*gate_for_task;
                    let released = lock.lock().expect("worker gate lock");
                    drop(
                        condition
                            .wait_while(released, |released| !*released)
                            .expect("worker gate wait"),
                    );
                    cancellation.is_cancelled()
                })
                .await
        });
        entered_rx.await.expect("blocking worker entered");
        workers.cancel();

        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), workers.drain())
                .await
                .is_err()
        );
        let (lock, condition) = &*gate;
        *lock.lock().expect("worker release lock") = true;
        condition.notify_one();
        workers.drain().await;
        assert_eq!(task.await.expect("worker task joined"), Ok(true));
    }

    #[tokio::test]
    async fn all_concurrent_drain_waiters_observe_worker_exit() {
        let workers = ManagedBlockingWorkers::new();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let gate_for_task = Arc::clone(&gate);
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let workers_for_task = workers.clone();
        let task = tokio::spawn(async move {
            workers_for_task
                .run(move |_| {
                    let _ = entered_tx.send(());
                    let (lock, condition) = &*gate_for_task;
                    let released = lock.lock().expect("worker gate lock");
                    drop(
                        condition
                            .wait_while(released, |released| !*released)
                            .expect("worker gate wait"),
                    );
                })
                .await
        });
        entered_rx.await.expect("blocking worker entered");
        let mut first = Box::pin(workers.drain());
        let mut second = Box::pin(workers.drain());
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut first)
                .await
                .is_err()
        );
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), &mut second)
                .await
                .is_err()
        );

        let (lock, condition) = &*gate;
        *lock.lock().expect("worker release lock") = true;
        condition.notify_one();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            tokio::join!(first, second);
            task.await
                .expect("worker task joined")
                .expect("worker result");
        })
        .await
        .expect("all drain waiters must observe zero");
    }
}
