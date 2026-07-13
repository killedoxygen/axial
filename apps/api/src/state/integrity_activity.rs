use super::ProducerLease;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::watch;

const INTEGRITY_ACTIVITY_LOCK_INVARIANT: &str =
    "integrity activity lock poisoned; sweep admission state may be inconsistent";

#[derive(Clone)]
pub(super) struct IntegrityActivityCoordinator {
    shared: Arc<IntegrityActivityShared>,
}

struct IntegrityActivityShared {
    state: Mutex<IntegrityActivityState>,
    changed: watch::Sender<IntegrityIdleSnapshot>,
}

struct IntegrityActivityState {
    phase: IntegrityActivityPhase,
    foreground_count: usize,
    idle_epoch: IntegrityIdleEpoch,
    next_sweep_id: u64,
    active_sweep: Option<ActiveIdleSweep>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IntegrityActivityPhase {
    Running,
    Closing,
}

struct ActiveIdleSweep {
    id: u64,
    epoch: IntegrityIdleEpoch,
    cancellation: IdleSweepCancellation,
    completion: Arc<IdleSweepCompletion>,
}

struct IdleSweepCompletion {
    settled: watch::Sender<bool>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntegrityIdleEpoch(u64);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IntegrityIdleSnapshot {
    epoch: IntegrityIdleEpoch,
    running: bool,
    foreground_count: usize,
    sweep_active: bool,
}

impl IntegrityIdleSnapshot {
    pub(crate) const fn epoch(self) -> IntegrityIdleEpoch {
        self.epoch
    }

    pub(crate) const fn is_stably_idle(self) -> bool {
        self.running && self.foreground_count == 0 && !self.sweep_active
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("integrity activity is closing")]
pub(crate) struct IntegrityActivityClosed;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum IdleSweepReserveError {
    #[error("integrity activity is closing")]
    Closing,
    #[error("the observed idle epoch is no longer current")]
    EpochChanged,
    #[error("foreground integrity activity is active")]
    ForegroundActive,
    #[error("an idle integrity sweep is already active")]
    SweepActive,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleSweepTerminal {
    Complete,
    Cancelled,
    Refused,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IdleSweepSettlement {
    Authoritative,
    Superseded,
}

#[derive(Clone)]
pub(crate) struct IdleSweepCancellation {
    cancelled: Arc<AtomicBool>,
}

impl IdleSweepCancellation {
    fn new() -> Self {
        Self {
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_for_test() -> Self {
        Self::new()
    }

    pub(crate) fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }
}

#[must_use]
pub(crate) struct IntegrityForegroundRegistration {
    coordinator: IntegrityActivityCoordinator,
    blocked_on: Option<Arc<IdleSweepCompletion>>,
    active: bool,
}

#[must_use]
pub(crate) struct IntegrityForegroundLease {
    coordinator: IntegrityActivityCoordinator,
    active: bool,
}

#[must_use]
pub(crate) struct IdleSweepReservation {
    coordinator: IntegrityActivityCoordinator,
    _producer: ProducerLease,
    id: u64,
    epoch: IntegrityIdleEpoch,
    cancellation: IdleSweepCancellation,
    active: bool,
}

impl IntegrityActivityCoordinator {
    pub(super) fn new() -> Self {
        let state = IntegrityActivityState {
            phase: IntegrityActivityPhase::Running,
            foreground_count: 0,
            idle_epoch: IntegrityIdleEpoch(0),
            next_sweep_id: 0,
            active_sweep: None,
        };
        let (changed, _) = watch::channel(state.snapshot());
        Self {
            shared: Arc::new(IntegrityActivityShared {
                state: Mutex::new(state),
                changed,
            }),
        }
    }

    pub(super) fn subscribe_idle(&self) -> watch::Receiver<IntegrityIdleSnapshot> {
        self.shared.changed.subscribe()
    }

    pub(super) fn register_foreground(
        &self,
    ) -> Result<IntegrityForegroundRegistration, IntegrityActivityClosed> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        if state.phase == IntegrityActivityPhase::Closing {
            return Err(IntegrityActivityClosed);
        }
        if state.foreground_count == 0 {
            state.advance_epoch();
        }
        state.foreground_count = state
            .foreground_count
            .checked_add(1)
            .expect("integrity foreground count overflowed");
        let blocked_on = state.active_sweep.as_ref().map(|sweep| {
            sweep.cancellation.cancel();
            sweep.completion.clone()
        });
        self.publish(&state);
        Ok(IntegrityForegroundRegistration {
            coordinator: self.clone(),
            blocked_on,
            active: true,
        })
    }

    pub(super) fn try_reserve_idle_sweep(
        &self,
        expected_epoch: IntegrityIdleEpoch,
        producer: ProducerLease,
    ) -> Result<IdleSweepReservation, IdleSweepReserveError> {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        if state.phase == IntegrityActivityPhase::Closing {
            return Err(IdleSweepReserveError::Closing);
        }
        if state.idle_epoch != expected_epoch {
            return Err(IdleSweepReserveError::EpochChanged);
        }
        if state.foreground_count != 0 {
            return Err(IdleSweepReserveError::ForegroundActive);
        }
        if state.active_sweep.is_some() {
            return Err(IdleSweepReserveError::SweepActive);
        }
        let id = state.next_sweep_id;
        state.next_sweep_id = state
            .next_sweep_id
            .checked_add(1)
            .expect("integrity sweep id overflowed");
        let cancellation = IdleSweepCancellation::new();
        let completion = Arc::new(IdleSweepCompletion::new());
        state.active_sweep = Some(ActiveIdleSweep {
            id,
            epoch: expected_epoch,
            cancellation: cancellation.clone(),
            completion,
        });
        self.publish(&state);
        Ok(IdleSweepReservation {
            coordinator: self.clone(),
            _producer: producer,
            id,
            epoch: expected_epoch,
            cancellation,
            active: true,
        })
    }

    pub(super) fn begin_shutdown(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        if state.phase == IntegrityActivityPhase::Closing {
            return;
        }
        state.phase = IntegrityActivityPhase::Closing;
        if let Some(sweep) = state.active_sweep.as_ref() {
            sweep.cancellation.cancel();
        }
        self.publish(&state);
    }

    fn reservation_is_current(
        &self,
        id: u64,
        epoch: IntegrityIdleEpoch,
        cancellation: &IdleSweepCancellation,
    ) -> bool {
        let state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        state.phase == IntegrityActivityPhase::Running
            && state.foreground_count == 0
            && state.idle_epoch == epoch
            && !cancellation.is_cancelled()
            && state
                .active_sweep
                .as_ref()
                .is_some_and(|sweep| sweep.id == id && sweep.epoch == epoch)
    }

    fn release_foreground(&self) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        state.foreground_count = state
            .foreground_count
            .checked_sub(1)
            .expect("integrity foreground lease released more than once");
        self.publish(&state);
    }

    fn finish_reservation(
        &self,
        id: u64,
        epoch: IntegrityIdleEpoch,
        terminal: IdleSweepTerminal,
    ) -> IdleSweepSettlement {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        let Some(active) = state
            .active_sweep
            .as_ref()
            .filter(|sweep| sweep.id == id && sweep.epoch == epoch)
        else {
            return IdleSweepSettlement::Superseded;
        };
        let authoritative = terminal == IdleSweepTerminal::Complete
            && state.phase == IntegrityActivityPhase::Running
            && state.foreground_count == 0
            && state.idle_epoch == epoch
            && !active.cancellation.is_cancelled();
        let active = state
            .active_sweep
            .take()
            .expect("matched integrity sweep disappeared");
        state.advance_epoch();
        active.completion.settle();
        self.publish(&state);
        if authoritative {
            IdleSweepSettlement::Authoritative
        } else {
            IdleSweepSettlement::Superseded
        }
    }

    fn abandon_reservation(&self, id: u64, epoch: IntegrityIdleEpoch) {
        let mut state = self
            .shared
            .state
            .lock()
            .expect(INTEGRITY_ACTIVITY_LOCK_INVARIANT);
        if !state
            .active_sweep
            .as_ref()
            .is_some_and(|sweep| sweep.id == id && sweep.epoch == epoch)
        {
            return;
        }
        let active = state
            .active_sweep
            .take()
            .expect("matched integrity sweep disappeared");
        active.cancellation.cancel();
        state.advance_epoch();
        active.completion.settle();
        self.publish(&state);
    }

    fn publish(&self, state: &IntegrityActivityState) {
        self.shared.changed.send_replace(state.snapshot());
    }
}

impl IntegrityActivityState {
    fn snapshot(&self) -> IntegrityIdleSnapshot {
        IntegrityIdleSnapshot {
            epoch: self.idle_epoch,
            running: self.phase == IntegrityActivityPhase::Running,
            foreground_count: self.foreground_count,
            sweep_active: self.active_sweep.is_some(),
        }
    }

    fn advance_epoch(&mut self) {
        self.idle_epoch.0 = self
            .idle_epoch
            .0
            .checked_add(1)
            .expect("integrity idle epoch overflowed");
    }
}

impl IdleSweepCompletion {
    fn new() -> Self {
        let (settled, _) = watch::channel(false);
        Self { settled }
    }

    fn settle(&self) {
        self.settled.send_replace(true);
    }

    async fn wait(&self) {
        let mut settled = self.settled.subscribe();
        loop {
            if *settled.borrow_and_update() {
                return;
            }
            settled
                .changed()
                .await
                .expect("idle sweep completion channel closed");
        }
    }
}

impl IntegrityForegroundRegistration {
    pub(crate) async fn wait_for_settlement(mut self) -> IntegrityForegroundLease {
        if let Some(completion) = self.blocked_on.take() {
            completion.wait().await;
        }
        self.active = false;
        IntegrityForegroundLease {
            coordinator: self.coordinator.clone(),
            active: true,
        }
    }
}

impl Drop for IntegrityForegroundRegistration {
    fn drop(&mut self) {
        if self.active {
            self.coordinator.release_foreground();
        }
    }
}

impl Drop for IntegrityForegroundLease {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.coordinator.release_foreground();
        }
    }
}

impl IdleSweepReservation {
    pub(crate) fn cancellation(&self) -> IdleSweepCancellation {
        self.cancellation.clone()
    }

    pub(crate) fn is_current(&self) -> bool {
        self.coordinator
            .reservation_is_current(self.id, self.epoch, &self.cancellation)
    }

    pub(crate) fn settle(mut self, terminal: IdleSweepTerminal) -> IdleSweepSettlement {
        self.active = false;
        self.coordinator
            .finish_reservation(self.id, self.epoch, terminal)
    }
}

impl Drop for IdleSweepReservation {
    fn drop(&mut self) {
        if self.active {
            self.active = false;
            self.coordinator.abandon_reservation(self.id, self.epoch);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn lifecycle_and_producer() -> (crate::state::AppLifecycle, ProducerLease) {
        let lifecycle = crate::state::AppLifecycle::new();
        let producer = lifecycle
            .try_claim_producer()
            .expect("claim idle sweep producer");
        (lifecycle, producer)
    }

    fn producer() -> ProducerLease {
        lifecycle_and_producer().1
    }

    trait AmbiguousIfClone<Marker> {
        fn assert_not_clone() {}
    }

    struct CloneMarker;

    impl<T: ?Sized> AmbiguousIfClone<()> for T {}
    impl<T: Clone> AmbiguousIfClone<CloneMarker> for T {}

    const _: fn() = || {
        let _ = <IdleSweepReservation as AmbiguousIfClone<_>>::assert_not_clone;
    };

    #[test]
    fn cancelled_foreground_registration_releases_its_idle_epoch() {
        let coordinator = IntegrityActivityCoordinator::new();
        let initial = *coordinator.subscribe_idle().borrow();
        let registration = coordinator.register_foreground().expect("registration");
        let active = *coordinator.subscribe_idle().borrow();
        assert!(!active.is_stably_idle());
        assert_ne!(active.epoch(), initial.epoch());

        drop(registration);

        let released = *coordinator.subscribe_idle().borrow();
        assert!(released.is_stably_idle());
        assert_eq!(released.epoch(), active.epoch());
    }

    #[tokio::test]
    async fn foreground_cancels_and_waits_for_the_exact_sweep() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");
        let cancellation = reservation.cancellation();
        let registration = coordinator.register_foreground().expect("registration");
        assert!(cancellation.is_cancelled());

        let waiter = tokio::spawn(registration.wait_for_settlement());
        tokio::task::yield_now().await;
        assert!(!waiter.is_finished());
        drop(reservation);
        let lease = tokio::time::timeout(Duration::from_millis(100), waiter)
            .await
            .expect("foreground settlement wait")
            .expect("foreground waiter");
        drop(lease);
        assert!(coordinator.subscribe_idle().borrow().is_stably_idle());
    }

    #[tokio::test]
    async fn two_foreground_registrations_share_exact_completion_after_one_is_cancelled() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");
        let first = coordinator
            .register_foreground()
            .expect("first registration");
        let second = coordinator
            .register_foreground()
            .expect("second registration");

        drop(first);
        let second = tokio::spawn(second.wait_for_settlement());
        tokio::task::yield_now().await;
        assert!(!second.is_finished());

        drop(reservation);
        let lease = tokio::time::timeout(Duration::from_millis(100), second)
            .await
            .expect("shared completion wait")
            .expect("foreground waiter");
        assert!(!coordinator.subscribe_idle().borrow().is_stably_idle());

        drop(lease);
        assert!(coordinator.subscribe_idle().borrow().is_stably_idle());
    }

    #[tokio::test]
    async fn stable_idle_publishes_only_after_the_last_foreground_holder_drops() {
        let coordinator = IntegrityActivityCoordinator::new();
        let initial_epoch = coordinator.subscribe_idle().borrow().epoch();
        let first = coordinator
            .register_foreground()
            .expect("first registration")
            .wait_for_settlement()
            .await;
        let second = coordinator
            .register_foreground()
            .expect("second registration")
            .wait_for_settlement()
            .await;
        let active_epoch = coordinator.subscribe_idle().borrow().epoch();
        assert_ne!(active_epoch, initial_epoch);

        drop(first);
        assert!(!coordinator.subscribe_idle().borrow().is_stably_idle());
        drop(second);

        let idle = *coordinator.subscribe_idle().borrow();
        assert!(idle.is_stably_idle());
        assert_eq!(idle.epoch(), active_epoch);
    }

    #[test]
    fn foreground_epoch_makes_late_completion_non_authoritative() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");
        let registration = coordinator.register_foreground().expect("registration");
        drop(registration);

        assert_eq!(
            reservation.settle(IdleSweepTerminal::Complete),
            IdleSweepSettlement::Superseded
        );
        let settled = *coordinator.subscribe_idle().borrow();
        assert!(settled.is_stably_idle());
        assert_ne!(settled.epoch(), epoch);
    }

    #[test]
    fn stale_settlement_cannot_affect_a_reservation_in_the_new_epoch() {
        let coordinator = IntegrityActivityCoordinator::new();
        let stale_epoch = coordinator.subscribe_idle().borrow().epoch();
        let stale = coordinator
            .try_reserve_idle_sweep(stale_epoch, producer())
            .expect("stale reservation");
        let stale_cancellation = stale.cancellation();
        let foreground = coordinator.register_foreground().expect("foreground");
        drop(foreground);
        assert_eq!(
            stale.settle(IdleSweepTerminal::Complete),
            IdleSweepSettlement::Superseded
        );

        let current_epoch = coordinator.subscribe_idle().borrow().epoch();
        let current = coordinator
            .try_reserve_idle_sweep(current_epoch, producer())
            .expect("current reservation");
        stale_cancellation.cancel();
        assert!(current.is_current());
        assert_eq!(
            current.settle(IdleSweepTerminal::Complete),
            IdleSweepSettlement::Authoritative
        );
    }

    #[test]
    fn settlement_advances_epoch_before_another_reservation() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");
        assert_eq!(
            reservation.settle(IdleSweepTerminal::Complete),
            IdleSweepSettlement::Authoritative
        );
        assert_eq!(
            coordinator.try_reserve_idle_sweep(epoch, producer()).err(),
            Some(IdleSweepReserveError::EpochChanged)
        );
    }

    #[test]
    fn late_idle_subscriber_observes_completed_settlement() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");
        assert_eq!(
            reservation.settle(IdleSweepTerminal::Complete),
            IdleSweepSettlement::Authoritative
        );

        let late = coordinator.subscribe_idle();
        assert!(late.borrow().is_stably_idle());
        assert_ne!(late.borrow().epoch(), epoch);
    }

    #[test]
    fn refused_settlement_is_non_authoritative_and_releases_the_epoch() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer())
            .expect("reservation");

        assert_eq!(
            reservation.settle(IdleSweepTerminal::Refused),
            IdleSweepSettlement::Superseded
        );
        let idle = *coordinator.subscribe_idle().borrow();
        assert!(idle.is_stably_idle());
        assert_ne!(idle.epoch(), epoch);
    }

    #[tokio::test]
    async fn shutdown_cancels_active_sweep_and_refuses_new_admission() {
        let coordinator = IntegrityActivityCoordinator::new();
        let epoch = coordinator.subscribe_idle().borrow().epoch();
        let (lifecycle, producer) = lifecycle_and_producer();
        let reservation = coordinator
            .try_reserve_idle_sweep(epoch, producer)
            .expect("reservation");
        let cancellation = reservation.cancellation();

        coordinator.begin_shutdown();

        assert!(cancellation.is_cancelled());
        assert_eq!(
            coordinator.register_foreground().err(),
            Some(IntegrityActivityClosed)
        );
        assert_eq!(
            coordinator
                .try_reserve_idle_sweep(
                    epoch,
                    lifecycle
                        .try_claim_producer()
                        .expect("claim rejected reservation producer"),
                )
                .err(),
            Some(IdleSweepReserveError::Closing)
        );
        let quiesce = tokio::spawn({
            let lifecycle = lifecycle.clone();
            async move { lifecycle.quiesce().await }
        });
        tokio::task::yield_now().await;
        assert!(!quiesce.is_finished());
        drop(reservation);
        tokio::time::timeout(Duration::from_millis(100), quiesce)
            .await
            .expect("producer drain wait")
            .expect("quiesce owner")
            .expect("quiesce succeeds");

        let rejected_lifecycle = crate::state::AppLifecycle::new();
        let rejected_producer = rejected_lifecycle
            .try_claim_producer()
            .expect("claim failed-admission producer");
        assert_eq!(
            coordinator
                .try_reserve_idle_sweep(epoch, rejected_producer)
                .err(),
            Some(IdleSweepReserveError::Closing)
        );
        tokio::time::timeout(Duration::from_millis(100), rejected_lifecycle.quiesce())
            .await
            .expect("failed admission releases producer")
            .expect("failed-admission lifecycle quiesces");
    }
}
