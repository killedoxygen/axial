use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd)]
pub(crate) struct ManagedArtifactMutationEpoch(u64);

#[derive(Debug, Default)]
struct ManagedArtifactMutationEpochState {
    current: u64,
    active: u64,
    exhausted: bool,
}

#[derive(Clone, Debug, Default)]
pub(super) struct ManagedArtifactMutationEpochCoordinator {
    state: Arc<Mutex<ManagedArtifactMutationEpochState>>,
}

#[derive(Debug)]
#[must_use = "managed artifact mutation admission must be retained across the effect"]
pub(crate) struct ManagedArtifactMutationAdmission {
    state: Arc<Mutex<ManagedArtifactMutationEpochState>>,
    _epoch: ManagedArtifactMutationEpoch,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("managed artifact mutation epoch is exhausted")]
pub(crate) struct ManagedArtifactMutationEpochExhausted;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum ManagedArtifactMutationEpochUnavailable {
    #[error("managed artifact mutation is in flight")]
    MutationInFlight,
    #[error("managed artifact mutation epoch changed")]
    EpochChanged,
    #[error(transparent)]
    Exhausted(#[from] ManagedArtifactMutationEpochExhausted),
}

impl ManagedArtifactMutationEpochCoordinator {
    pub(super) fn current(
        &self,
    ) -> Result<ManagedArtifactMutationEpoch, ManagedArtifactMutationEpochExhausted> {
        let state = self.lock();
        if state.exhausted {
            return Err(ManagedArtifactMutationEpochExhausted);
        }
        Ok(ManagedArtifactMutationEpoch(state.current))
    }

    pub(super) fn capture(
        &self,
    ) -> Result<ManagedArtifactMutationEpoch, ManagedArtifactMutationEpochUnavailable> {
        let state = self.lock();
        if state.exhausted {
            return Err(ManagedArtifactMutationEpochExhausted.into());
        }
        if state.active != 0 {
            return Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight);
        }
        Ok(ManagedArtifactMutationEpoch(state.current))
    }

    pub(super) fn admit(
        &self,
    ) -> Result<ManagedArtifactMutationAdmission, ManagedArtifactMutationEpochExhausted> {
        let mut state = self.lock();
        let epoch = advance(&mut state)?;
        drop(state);
        Ok(ManagedArtifactMutationAdmission {
            state: self.state.clone(),
            _epoch: epoch,
        })
    }

    pub(super) fn admit_from_expected(
        &self,
        expected: &AtomicU64,
    ) -> Result<ManagedArtifactMutationAdmission, ManagedArtifactMutationEpochUnavailable> {
        let mut state = self.lock();
        if state.exhausted {
            return Err(ManagedArtifactMutationEpochExhausted.into());
        }
        if state.active != 0 {
            return Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight);
        }
        if expected.load(Ordering::Acquire) != state.current {
            return Err(ManagedArtifactMutationEpochUnavailable::EpochChanged);
        }
        let epoch = advance(&mut state)?;
        expected.store(epoch.value(), Ordering::Release);
        drop(state);
        Ok(ManagedArtifactMutationAdmission {
            state: self.state.clone(),
            _epoch: epoch,
        })
    }

    fn lock(&self) -> MutexGuard<'_, ManagedArtifactMutationEpochState> {
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg(test)]
    fn with_epoch(epoch: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(ManagedArtifactMutationEpochState {
                current: epoch,
                ..ManagedArtifactMutationEpochState::default()
            })),
        }
    }
}

fn advance(
    state: &mut ManagedArtifactMutationEpochState,
) -> Result<ManagedArtifactMutationEpoch, ManagedArtifactMutationEpochExhausted> {
    if state.exhausted {
        return Err(ManagedArtifactMutationEpochExhausted);
    }
    let Some(next) = state.current.checked_add(1) else {
        state.exhausted = true;
        return Err(ManagedArtifactMutationEpochExhausted);
    };
    let Some(active) = state.active.checked_add(1) else {
        state.exhausted = true;
        return Err(ManagedArtifactMutationEpochExhausted);
    };
    state.current = next;
    state.active = active;
    Ok(ManagedArtifactMutationEpoch(next))
}

impl ManagedArtifactMutationEpoch {
    pub(super) const fn value(self) -> u64 {
        self.0
    }
}

impl Drop for ManagedArtifactMutationAdmission {
    fn drop(&mut self) {
        let mut state = self.state.lock().unwrap_or_else(|error| error.into_inner());
        debug_assert!(state.active > 0, "managed mutation admission underflow");
        state.active = state.active.saturating_sub(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_admission_retains_the_epoch_bump() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::default();

        let admission = coordinator.admit().expect("first mutation admission");
        assert_eq!(coordinator.current(), Ok(ManagedArtifactMutationEpoch(1)));
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        );
        drop(admission);

        assert_eq!(coordinator.current(), Ok(ManagedArtifactMutationEpoch(1)));
        assert_eq!(coordinator.capture(), Ok(ManagedArtifactMutationEpoch(1)));
    }

    #[test]
    fn overlapping_writers_never_open_a_capture_window() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::default();
        let first = coordinator.admit().expect("first mutation admission");
        let second = coordinator.admit().expect("second mutation admission");

        drop(first);
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        );
        drop(second);

        assert_eq!(coordinator.capture(), Ok(ManagedArtifactMutationEpoch(2)));
    }

    #[test]
    fn conditional_handoff_rejects_an_active_or_earlier_writer_without_resurrection() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::default();
        let expected = AtomicU64::new(0);
        let earlier = coordinator.admit().expect("earlier writer");

        assert!(matches!(
            coordinator.admit_from_expected(&expected),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        ));
        assert_eq!(expected.load(Ordering::Acquire), 0);
        drop(earlier);

        assert!(matches!(
            coordinator.admit_from_expected(&expected),
            Err(ManagedArtifactMutationEpochUnavailable::EpochChanged)
        ));
        assert_eq!(expected.load(Ordering::Acquire), 0);
        assert_eq!(coordinator.capture(), Ok(ManagedArtifactMutationEpoch(1)));
    }

    #[test]
    fn conditional_handoff_advances_and_activates_in_one_transaction() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::default();
        let expected = AtomicU64::new(0);

        let mutation = coordinator
            .admit_from_expected(&expected)
            .expect("exact handoff");

        assert_eq!(expected.load(Ordering::Acquire), 1);
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        );
        drop(mutation);
        assert_eq!(coordinator.capture(), Ok(ManagedArtifactMutationEpoch(1)));
    }

    #[test]
    fn racing_conditional_handoffs_cannot_resurrect_the_loser() {
        use std::sync::{Barrier, mpsc};

        let coordinator = ManagedArtifactMutationEpochCoordinator::default();
        let expectations = [Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0))];
        let start = Arc::new(Barrier::new(3));
        let release_winner = Arc::new(Barrier::new(2));
        let (outcomes_tx, outcomes_rx) = mpsc::channel();
        let mut workers = Vec::new();

        for (index, expected) in expectations.iter().cloned().enumerate() {
            let coordinator = coordinator.clone();
            let start = start.clone();
            let release_winner = release_winner.clone();
            let outcomes_tx = outcomes_tx.clone();
            workers.push(std::thread::spawn(move || {
                start.wait();
                match coordinator.admit_from_expected(&expected) {
                    Ok(admission) => {
                        outcomes_tx.send((index, None)).expect("report winner");
                        release_winner.wait();
                        drop(admission);
                    }
                    Err(error) => outcomes_tx
                        .send((index, Some(error)))
                        .expect("report loser"),
                }
            }));
        }
        drop(outcomes_tx);
        start.wait();

        let first = outcomes_rx.recv().expect("first race outcome");
        let second = outcomes_rx.recv().expect("second race outcome");
        let winner = [first, second]
            .into_iter()
            .find_map(|(index, error)| error.is_none().then_some(index))
            .expect("one conditional handoff wins");
        let loser = 1 - winner;
        assert!(
            [first, second].into_iter().any(|(index, error)| {
                index == loser
                    && matches!(
                        error,
                        Some(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
                            | Some(ManagedArtifactMutationEpochUnavailable::EpochChanged)
                    )
            }),
            "the losing snapshot must be rejected"
        );

        release_winner.wait();
        for worker in workers {
            worker.join().expect("conditional handoff worker");
        }
        assert_eq!(expectations[winner].load(Ordering::Acquire), 1);
        assert_eq!(expectations[loser].load(Ordering::Acquire), 0);
        assert!(
            matches!(
                coordinator.admit_from_expected(&expectations[loser]),
                Err(ManagedArtifactMutationEpochUnavailable::EpochChanged)
            ),
            "a losing verification snapshot must stay stale after the winner settles"
        );
    }

    #[test]
    fn cloned_coordinators_share_one_global_epoch() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::default();
        let shared_root_writer = coordinator.clone();

        let mutation = shared_root_writer
            .admit()
            .expect("shared-root mutation admission");

        assert_eq!(coordinator.current(), Ok(ManagedArtifactMutationEpoch(1)));
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        );
        drop(mutation);
        assert_eq!(coordinator.capture(), Ok(ManagedArtifactMutationEpoch(1)));
    }

    #[test]
    fn exhausted_epoch_never_wraps_or_reopens() {
        let coordinator = ManagedArtifactMutationEpochCoordinator::with_epoch(u64::MAX - 1);

        let final_admission = coordinator.admit().expect("final mutation admission");
        assert_eq!(
            coordinator.current(),
            Ok(ManagedArtifactMutationEpoch(u64::MAX))
        );
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::MutationInFlight)
        );
        assert!(matches!(
            coordinator.admit(),
            Err(ManagedArtifactMutationEpochExhausted)
        ));
        drop(final_admission);
        assert_eq!(
            coordinator.current(),
            Err(ManagedArtifactMutationEpochExhausted)
        );
        assert_eq!(
            coordinator.capture(),
            Err(ManagedArtifactMutationEpochUnavailable::Exhausted(
                ManagedArtifactMutationEpochExhausted
            ))
        );
        assert!(matches!(
            coordinator.admit(),
            Err(ManagedArtifactMutationEpochExhausted)
        ));
    }
}
