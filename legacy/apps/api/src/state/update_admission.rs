use std::sync::{Arc, Mutex};

const UPDATE_ADMISSION_LOCK_POISONED: &str =
    "update admission lock poisoned; runtime operation authority may be inconsistent";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UpdateAdmissionPhase {
    Open,
    Applying,
    RestartPending,
}

#[derive(Debug)]
struct UpdateAdmissionState {
    phase: UpdateAdmissionPhase,
    active_operations: usize,
}

#[derive(Debug)]
struct UpdateAdmissionInner {
    state: Mutex<UpdateAdmissionState>,
}

#[derive(Clone, Debug)]
pub(super) struct UpdateAdmissionCoordinator {
    inner: Arc<UpdateAdmissionInner>,
}

#[derive(Clone, Debug)]
pub(crate) struct UpdateOperationLease {
    _inner: Arc<UpdateOperationLeaseInner>,
}

#[derive(Debug)]
struct UpdateOperationLeaseInner {
    owner: Arc<UpdateAdmissionInner>,
}

#[derive(Debug)]
pub(crate) struct UpdateApplyAuthority {
    owner: Arc<UpdateAdmissionInner>,
    restart_pending: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateOperationAdmissionError {
    ApplyInProgress,
    RestartPending,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum UpdateApplyAdmissionError {
    ActiveOperations,
    ApplyInProgress,
    RestartPending,
}

impl UpdateAdmissionCoordinator {
    pub(super) fn new() -> Self {
        Self {
            inner: Arc::new(UpdateAdmissionInner {
                state: Mutex::new(UpdateAdmissionState {
                    phase: UpdateAdmissionPhase::Open,
                    active_operations: 0,
                }),
            }),
        }
    }

    pub(super) fn try_admit_operation(
        &self,
    ) -> Result<UpdateOperationLease, UpdateOperationAdmissionError> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect(UPDATE_ADMISSION_LOCK_POISONED);
        match state.phase {
            UpdateAdmissionPhase::Open => {}
            UpdateAdmissionPhase::Applying => {
                return Err(UpdateOperationAdmissionError::ApplyInProgress);
            }
            UpdateAdmissionPhase::RestartPending => {
                return Err(UpdateOperationAdmissionError::RestartPending);
            }
        }
        state.active_operations = state
            .active_operations
            .checked_add(1)
            .expect("active update admission operation count overflowed");
        drop(state);
        Ok(UpdateOperationLease {
            _inner: Arc::new(UpdateOperationLeaseInner {
                owner: self.inner.clone(),
            }),
        })
    }

    pub(super) fn try_begin_apply(
        &self,
    ) -> Result<UpdateApplyAuthority, UpdateApplyAdmissionError> {
        let mut state = self
            .inner
            .state
            .lock()
            .expect(UPDATE_ADMISSION_LOCK_POISONED);
        match state.phase {
            UpdateAdmissionPhase::Open => {}
            UpdateAdmissionPhase::Applying => {
                return Err(UpdateApplyAdmissionError::ApplyInProgress);
            }
            UpdateAdmissionPhase::RestartPending => {
                return Err(UpdateApplyAdmissionError::RestartPending);
            }
        }
        if state.active_operations != 0 {
            return Err(UpdateApplyAdmissionError::ActiveOperations);
        }
        state.phase = UpdateAdmissionPhase::Applying;
        drop(state);
        Ok(UpdateApplyAuthority {
            owner: self.inner.clone(),
            restart_pending: false,
        })
    }
}

impl UpdateApplyAuthority {
    pub(crate) fn mark_restart_pending(mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .expect(UPDATE_ADMISSION_LOCK_POISONED);
        assert_eq!(
            state.phase,
            UpdateAdmissionPhase::Applying,
            "update apply authority lost exclusive admission"
        );
        assert_eq!(
            state.active_operations, 0,
            "runtime operation admitted during update apply"
        );
        state.phase = UpdateAdmissionPhase::RestartPending;
        self.restart_pending = true;
    }
}

impl Drop for UpdateApplyAuthority {
    fn drop(&mut self) {
        if self.restart_pending {
            return;
        }
        let mut state = self
            .owner
            .state
            .lock()
            .expect(UPDATE_ADMISSION_LOCK_POISONED);
        if state.phase == UpdateAdmissionPhase::Applying {
            state.phase = UpdateAdmissionPhase::Open;
        }
    }
}

impl Drop for UpdateOperationLeaseInner {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .expect(UPDATE_ADMISSION_LOCK_POISONED);
        state.active_operations = state
            .active_operations
            .checked_sub(1)
            .expect("released update operation admission that was not active");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Barrier;

    #[test]
    fn active_operation_and_apply_are_one_atomic_admission_boundary() {
        let coordinator = UpdateAdmissionCoordinator::new();
        let operation = coordinator
            .try_admit_operation()
            .expect("admit runtime operation");

        assert_eq!(
            coordinator.try_begin_apply().unwrap_err(),
            UpdateApplyAdmissionError::ActiveOperations
        );
        drop(operation);

        let apply = coordinator.try_begin_apply().expect("begin update apply");
        assert_eq!(
            coordinator.try_admit_operation().unwrap_err(),
            UpdateOperationAdmissionError::ApplyInProgress
        );
        drop(apply);
        coordinator
            .try_admit_operation()
            .expect("failed apply reopens admission");
    }

    #[test]
    fn cloned_operation_lease_retains_one_admission_until_every_owner_drops() {
        let coordinator = UpdateAdmissionCoordinator::new();
        let operation = coordinator
            .try_admit_operation()
            .expect("admit runtime operation");
        let retained = operation.clone();
        drop(operation);

        assert_eq!(
            coordinator.try_begin_apply().unwrap_err(),
            UpdateApplyAdmissionError::ActiveOperations
        );
        drop(retained);
        coordinator.try_begin_apply().expect("last owner releases");
    }

    #[test]
    fn racing_operation_and_apply_never_both_acquire_authority() {
        for _ in 0..256 {
            let coordinator = UpdateAdmissionCoordinator::new();
            let start = Arc::new(Barrier::new(3));
            let finish = Arc::new(Barrier::new(2));

            let operation_coordinator = coordinator.clone();
            let operation_start = start.clone();
            let operation_finish = finish.clone();
            let operation = std::thread::spawn(move || {
                operation_start.wait();
                let lease = operation_coordinator.try_admit_operation().ok();
                operation_finish.wait();
                lease.is_some()
            });

            let apply_coordinator = coordinator.clone();
            let apply_start = start.clone();
            let apply_finish = finish.clone();
            let apply = std::thread::spawn(move || {
                apply_start.wait();
                let authority = apply_coordinator.try_begin_apply().ok();
                apply_finish.wait();
                authority.is_some()
            });

            start.wait();
            let operation_admitted = operation.join().expect("operation race thread");
            let apply_admitted = apply.join().expect("apply race thread");
            assert_ne!(
                operation_admitted, apply_admitted,
                "exactly one side of the admission race must acquire authority"
            );
        }
    }

    #[test]
    fn restart_pending_permanently_closes_runtime_operation_admission() {
        let coordinator = UpdateAdmissionCoordinator::new();
        coordinator
            .try_begin_apply()
            .expect("begin update apply")
            .mark_restart_pending();

        assert_eq!(
            coordinator.try_admit_operation().unwrap_err(),
            UpdateOperationAdmissionError::RestartPending
        );
        assert_eq!(
            coordinator.try_begin_apply().unwrap_err(),
            UpdateApplyAdmissionError::RestartPending
        );
    }
}
