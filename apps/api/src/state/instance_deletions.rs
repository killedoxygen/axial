use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};
use tokio::sync::{Mutex as AsyncMutex, OwnedMutexGuard};

#[derive(Clone)]
pub(super) struct InstanceDeletionCoordinator {
    gate: Arc<AsyncMutex<()>>,
    phase: Arc<AtomicU8>,
}

#[must_use = "instance deletion admission must be retained through transaction settlement"]
pub(super) struct InstanceDeletionAdmission {
    _gate: OwnedMutexGuard<()>,
}

#[must_use = "instance deletion close must be finished only after retained transactions settle"]
pub(super) struct InstanceDeletionCloseAdmission {
    phase: Arc<AtomicU8>,
    _gate: OwnedMutexGuard<()>,
    finished: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum InstanceDeletionPhase {
    Running = 0,
    Closing = 1,
    Closed = 2,
}

impl InstanceDeletionCoordinator {
    pub(super) fn new() -> Self {
        Self {
            gate: Arc::new(AsyncMutex::new(())),
            phase: Arc::new(AtomicU8::new(InstanceDeletionPhase::Running as u8)),
        }
    }

    pub(super) async fn admit(&self) -> io::Result<InstanceDeletionAdmission> {
        let gate = Arc::clone(&self.gate).lock_owned().await;
        if self.phase() != InstanceDeletionPhase::Running {
            return Err(instance_deletion_closed_error());
        }
        Ok(InstanceDeletionAdmission { _gate: gate })
    }

    pub(super) async fn begin_close(&self) -> io::Result<InstanceDeletionCloseAdmission> {
        let gate = Arc::clone(&self.gate).lock_owned().await;
        match self.phase() {
            InstanceDeletionPhase::Closed => {
                return Ok(InstanceDeletionCloseAdmission {
                    phase: Arc::clone(&self.phase),
                    _gate: gate,
                    finished: true,
                });
            }
            InstanceDeletionPhase::Running | InstanceDeletionPhase::Closing => {
                self.phase
                    .store(InstanceDeletionPhase::Closing as u8, Ordering::Release);
            }
        }
        Ok(InstanceDeletionCloseAdmission {
            phase: Arc::clone(&self.phase),
            _gate: gate,
            finished: false,
        })
    }

    fn phase(&self) -> InstanceDeletionPhase {
        match self.phase.load(Ordering::Acquire) {
            value if value == InstanceDeletionPhase::Running as u8 => {
                InstanceDeletionPhase::Running
            }
            value if value == InstanceDeletionPhase::Closing as u8 => {
                InstanceDeletionPhase::Closing
            }
            value if value == InstanceDeletionPhase::Closed as u8 => {
                InstanceDeletionPhase::Closed
            }
            _ => panic!("instance deletion coordinator phase is invalid"),
        }
    }
}

impl InstanceDeletionCloseAdmission {
    pub(super) fn finish(mut self) {
        if !self.finished {
            self.phase
                .store(InstanceDeletionPhase::Closed as u8, Ordering::Release);
            self.finished = true;
        }
    }
}

impl Drop for InstanceDeletionCloseAdmission {
    fn drop(&mut self) {
        if !self.finished {
            self.phase
                .store(InstanceDeletionPhase::Running as u8, Ordering::Release);
        }
    }
}

fn instance_deletion_closed_error() -> io::Error {
    io::Error::other("instance deletion coordinator is closed")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn close_waits_for_the_exact_in_flight_deletion() {
        let coordinator = InstanceDeletionCoordinator::new();
        let deletion = coordinator.admit().await.expect("admit deletion");
        let closing = coordinator.clone();
        let close = tokio::spawn(async move { closing.begin_close().await });
        tokio::task::yield_now().await;
        assert!(!close.is_finished());

        drop(deletion);
        close
            .await
            .expect("join close")
            .expect("begin close")
            .finish();
        assert!(coordinator.admit().await.is_err());
    }

    #[tokio::test]
    async fn failed_close_reopens_admission_for_shutdown_retry() {
        let coordinator = InstanceDeletionCoordinator::new();
        drop(coordinator.begin_close().await.expect("begin failed close"));
        drop(coordinator.admit().await.expect("retry admission"));

        coordinator
            .begin_close()
            .await
            .expect("begin successful close")
            .finish();
        assert!(coordinator.admit().await.is_err());
    }
}
