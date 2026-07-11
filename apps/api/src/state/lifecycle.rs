use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::watch;

const LIFECYCLE_LOCK_INVARIANT: &str =
    "application lifecycle lock poisoned; shutdown admission state may be inconsistent";
const LIFECYCLE_QUIESCE_DEADLINE: Duration = Duration::from_secs(10);

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub(crate) enum AppLifecyclePhase {
    Running,
    DrainingRequests,
    QuiescingProducers,
    Quiesced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("application shutdown is in progress")]
pub(crate) struct LifecycleAdmissionError;

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("application shutdown quiescence is incomplete")]
pub(crate) struct LifecycleQuiesceError;

#[derive(Clone)]
pub(crate) struct AppLifecycle {
    shared: Arc<AppLifecycleShared>,
}

struct AppLifecycleShared {
    state: Mutex<AppLifecycleState>,
    changed: watch::Sender<LifecycleSnapshot>,
    shutdown: watch::Sender<bool>,
}

struct AppLifecycleState {
    phase: AppLifecyclePhase,
    active_requests: usize,
    active_producers: usize,
    quiesce_owner_started: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct LifecycleSnapshot {
    phase: AppLifecyclePhase,
    active_requests: usize,
    active_producers: usize,
}

impl AppLifecycleState {
    fn snapshot(&self) -> LifecycleSnapshot {
        LifecycleSnapshot {
            phase: self.phase,
            active_requests: self.active_requests,
            active_producers: self.active_producers,
        }
    }
}

#[must_use]
pub(crate) struct RequestLease {
    lifecycle: Option<AppLifecycle>,
    handoff_authorization: Arc<Mutex<bool>>,
}

#[must_use]
pub(crate) struct ProducerLease {
    lifecycle: Option<AppLifecycle>,
}

#[derive(Clone)]
pub(crate) struct RequestProducerHandoff {
    lifecycle: AppLifecycle,
    authorization: Arc<Mutex<bool>>,
}

impl AppLifecycle {
    pub(crate) fn new() -> Self {
        let (shutdown, _) = watch::channel(false);
        let state = AppLifecycleState {
            phase: AppLifecyclePhase::Running,
            active_requests: 0,
            active_producers: 0,
            quiesce_owner_started: false,
        };
        let (changed, _) = watch::channel(state.snapshot());
        Self {
            shared: Arc::new(AppLifecycleShared {
                state: Mutex::new(state),
                changed,
                shutdown,
            }),
        }
    }

    pub(crate) fn try_admit_request(&self) -> Result<RequestLease, LifecycleAdmissionError> {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        if state.phase != AppLifecyclePhase::Running {
            return Err(LifecycleAdmissionError);
        }
        state.active_requests = state
            .active_requests
            .checked_add(1)
            .expect("active request count overflowed");
        self.publish(&state);
        Ok(RequestLease {
            lifecycle: Some(self.clone()),
            handoff_authorization: Arc::new(Mutex::new(true)),
        })
    }

    pub(crate) fn try_claim_producer(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        if state.phase != AppLifecyclePhase::Running {
            return Err(LifecycleAdmissionError);
        }
        Ok(self.claim_producer(&mut state))
    }

    pub(crate) fn subscribe_shutdown(&self) -> watch::Receiver<bool> {
        self.shared.shutdown.subscribe()
    }

    pub(crate) async fn quiesce(&self) -> Result<(), LifecycleQuiesceError> {
        self.quiesce_with_deadline(LIFECYCLE_QUIESCE_DEADLINE).await
    }

    async fn quiesce_with_deadline(&self, deadline: Duration) -> Result<(), LifecycleQuiesceError> {
        let should_spawn = {
            let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
            if state.phase == AppLifecyclePhase::Running {
                state.phase = AppLifecyclePhase::DrainingRequests;
            }
            if state.phase == AppLifecyclePhase::DrainingRequests && !state.quiesce_owner_started {
                state.quiesce_owner_started = true;
                self.publish(&state);
                true
            } else {
                self.publish(&state);
                false
            }
        };

        if should_spawn {
            let lifecycle = self.clone();
            tokio::spawn(async move {
                lifecycle.coordinate_quiesce().await;
            });
        }

        tokio::time::timeout(deadline, self.wait_for_phase(AppLifecyclePhase::Quiesced))
            .await
            .map_err(|_| LifecycleQuiesceError)
    }

    #[cfg(test)]
    pub(crate) fn phase(&self) -> AppLifecyclePhase {
        self.shared
            .state
            .lock()
            .expect(LIFECYCLE_LOCK_INVARIANT)
            .phase
    }

    async fn coordinate_quiesce(&self) {
        self.wait_for_requests_to_drain().await;
        {
            let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
            if state.phase == AppLifecyclePhase::DrainingRequests {
                state.phase = AppLifecyclePhase::QuiescingProducers;
                self.shared.shutdown.send_replace(true);
                self.publish(&state);
            }
        }

        self.wait_for_producers_to_drain().await;
        {
            let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
            if state.phase == AppLifecyclePhase::QuiescingProducers {
                state.phase = AppLifecyclePhase::Quiesced;
                self.publish(&state);
            }
        }
    }

    async fn wait_for_requests_to_drain(&self) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            if changed.borrow_and_update().active_requests == 0 {
                return;
            }
            changed
                .changed()
                .await
                .expect("application lifecycle change channel closed");
        }
    }

    async fn wait_for_producers_to_drain(&self) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            if changed.borrow_and_update().active_producers == 0 {
                return;
            }
            changed
                .changed()
                .await
                .expect("application lifecycle change channel closed");
        }
    }

    async fn wait_for_phase(&self, expected: AppLifecyclePhase) {
        let mut changed = self.shared.changed.subscribe();
        loop {
            if changed.borrow_and_update().phase >= expected {
                return;
            }
            changed
                .changed()
                .await
                .expect("application lifecycle change channel closed");
        }
    }

    fn release_request(&self) {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        state.active_requests = state
            .active_requests
            .checked_sub(1)
            .expect("released an application request lease that was not acquired");
        self.publish(&state);
    }

    fn try_claim_request_producer(
        &self,
        authorization: &Mutex<bool>,
    ) -> Result<ProducerLease, LifecycleAdmissionError> {
        let authorized = authorization.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        if !*authorized {
            return Err(LifecycleAdmissionError);
        }
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        if !matches!(
            state.phase,
            AppLifecyclePhase::Running | AppLifecyclePhase::DrainingRequests
        ) {
            return Err(LifecycleAdmissionError);
        }
        Ok(self.claim_producer(&mut state))
    }

    fn claim_producer(&self, state: &mut AppLifecycleState) -> ProducerLease {
        state.active_producers = state
            .active_producers
            .checked_add(1)
            .expect("active producer count overflowed");
        self.publish(state);
        ProducerLease {
            lifecycle: Some(self.clone()),
        }
    }

    fn release_producer(&self) {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        state.active_producers = state
            .active_producers
            .checked_sub(1)
            .expect("released an application producer lease that was not acquired");
        self.publish(&state);
    }

    fn claim_authorized_child(&self) -> ProducerLease {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        assert!(
            state.active_producers > 0,
            "authorized child claim requires a live producer lease"
        );
        assert!(
            state.phase <= AppLifecyclePhase::QuiescingProducers,
            "authorized child claim cannot follow producer quiescence"
        );
        state.active_producers = state
            .active_producers
            .checked_add(1)
            .expect("active producer count overflowed");
        self.publish(&state);
        ProducerLease {
            lifecycle: Some(self.clone()),
        }
    }

    fn try_claim_authorized_successor(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        let mut state = self.shared.state.lock().expect(LIFECYCLE_LOCK_INVARIANT);
        if state.active_producers == 0 || state.phase != AppLifecyclePhase::Running {
            return Err(LifecycleAdmissionError);
        }
        Ok(self.claim_producer(&mut state))
    }

    fn publish(&self, state: &AppLifecycleState) {
        self.shared.changed.send_replace(state.snapshot());
    }
}

impl ProducerLease {
    pub(crate) fn claim_child(&self) -> ProducerLease {
        self.lifecycle
            .as_ref()
            .expect("producer lease was already consumed")
            .claim_authorized_child()
    }

    pub(crate) fn try_claim_successor(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        self.lifecycle
            .as_ref()
            .expect("producer lease was already consumed")
            .try_claim_authorized_successor()
    }

    pub(crate) fn spawn_child<F>(&self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        self.claim_child().spawn(future);
    }

    pub(crate) fn spawn<F>(mut self, future: F)
    where
        F: Future<Output = ()> + Send + 'static,
    {
        let lease = ProducerLease {
            lifecycle: self.lifecycle.take(),
        };
        tokio::spawn(async move {
            let _lease = lease;
            future.await;
        });
    }

    pub(crate) fn spawn_joinable<F, T>(mut self, future: F) -> tokio::task::JoinHandle<T>
    where
        F: Future<Output = T> + Send + 'static,
        T: Send + 'static,
    {
        let lease = ProducerLease {
            lifecycle: self.lifecycle.take(),
        };
        tokio::spawn(async move {
            let _lease = lease;
            future.await
        })
    }
}

impl RequestLease {
    pub(crate) fn producer_handoff(&self) -> RequestProducerHandoff {
        RequestProducerHandoff {
            lifecycle: self
                .lifecycle
                .as_ref()
                .expect("request lease was already consumed")
                .clone(),
            authorization: self.handoff_authorization.clone(),
        }
    }
}

impl RequestProducerHandoff {
    pub(crate) fn try_claim(&self) -> Result<ProducerLease, LifecycleAdmissionError> {
        self.lifecycle
            .try_claim_request_producer(&self.authorization)
    }
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        if let Some(lifecycle) = self.lifecycle.take() {
            *self
                .handoff_authorization
                .lock()
                .expect(LIFECYCLE_LOCK_INVARIANT) = false;
            lifecycle.release_request();
        }
    }
}

impl Drop for ProducerLease {
    fn drop(&mut self) {
        if let Some(lifecycle) = self.lifecycle.take() {
            lifecycle.release_producer();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[tokio::test]
    async fn request_drain_precedes_producer_quiescence() {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let handoff = request.producer_handoff();
        let lifecycle_task = lifecycle.clone();
        let quiesce = tokio::spawn(async move { lifecycle_task.quiesce().await });

        wait_for_phase(&lifecycle, AppLifecyclePhase::DrainingRequests).await;
        assert_eq!(
            lifecycle.try_admit_request().err(),
            Some(LifecycleAdmissionError)
        );
        assert_eq!(
            lifecycle.try_claim_producer().err(),
            Some(LifecycleAdmissionError)
        );
        let producer = handoff
            .try_claim()
            .expect("live request can hand off producer while requests drain");
        assert!(!*lifecycle.subscribe_shutdown().borrow());

        drop(request);
        wait_for_phase(&lifecycle, AppLifecyclePhase::QuiescingProducers).await;
        assert!(*lifecycle.subscribe_shutdown().borrow());
        assert_eq!(
            lifecycle.try_claim_producer().err(),
            Some(LifecycleAdmissionError)
        );
        assert!(!quiesce.is_finished());

        drop(producer);
        quiesce
            .await
            .expect("quiesce owner joins")
            .expect("quiesce completes");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
        assert_eq!(handoff.try_claim().err(), Some(LifecycleAdmissionError));
    }

    #[tokio::test]
    async fn cancelled_quiesce_caller_does_not_cancel_shared_transition() {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let lifecycle_task = lifecycle.clone();
        let caller = tokio::spawn(async move { lifecycle_task.quiesce().await });

        wait_for_phase(&lifecycle, AppLifecyclePhase::DrainingRequests).await;
        caller.abort();
        assert!(caller.await.expect_err("caller cancelled").is_cancelled());
        drop(request);

        tokio::time::timeout(Duration::from_secs(1), lifecycle.quiesce())
            .await
            .expect("shared transition completes after caller cancellation")
            .expect("quiesce completes");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
    }

    #[tokio::test]
    async fn concurrent_and_repeated_quiesce_calls_share_one_transition() {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let producer = lifecycle.try_claim_producer().expect("claim producer");
        let first = {
            let lifecycle = lifecycle.clone();
            tokio::spawn(async move { lifecycle.quiesce().await })
        };
        wait_for_phase(&lifecycle, AppLifecyclePhase::DrainingRequests).await;
        let mut concurrent = Vec::new();
        for _ in 0..16 {
            let lifecycle = lifecycle.clone();
            concurrent.push(tokio::spawn(async move { lifecycle.quiesce().await }));
        }

        drop(request);
        wait_for_phase(&lifecycle, AppLifecyclePhase::QuiescingProducers).await;
        drop(producer);
        first
            .await
            .expect("first quiesce waiter")
            .expect("first quiesce completes");
        for waiter in concurrent {
            waiter
                .await
                .expect("concurrent quiesce waiter")
                .expect("concurrent quiesce completes");
        }
        lifecycle
            .quiesce()
            .await
            .expect("repeated quiesce completes");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
    }

    #[tokio::test]
    async fn live_producer_can_handoff_child_after_shutdown_signal() {
        let lifecycle = AppLifecycle::new();
        let parent = lifecycle.try_claim_producer().expect("claim parent");
        let mut shutdown = lifecycle.subscribe_shutdown();
        let lifecycle_task = lifecycle.clone();
        let quiesce = tokio::spawn(async move { lifecycle_task.quiesce().await });

        while !*shutdown.borrow_and_update() {
            shutdown.changed().await.expect("shutdown signal");
        }
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::QuiescingProducers);
        let child_shutdown = shutdown;
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        parent.spawn_child(async move {
            assert!(*child_shutdown.borrow());
            let _ = started_tx.send(());
            let _ = release_rx.await;
        });
        started_rx.await.expect("authorized child started");
        drop(parent);
        assert!(!quiesce.is_finished());

        release_tx.send(()).expect("release authorized child");
        quiesce
            .await
            .expect("quiesce waits for child")
            .expect("quiesce completes");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
    }

    #[tokio::test(start_paused = true)]
    async fn held_request_times_out_without_canceling_later_quiescence() {
        let lifecycle = AppLifecycle::new();
        let request = lifecycle.try_admit_request().expect("admit request");
        let lifecycle_task = lifecycle.clone();
        let waiter = tokio::spawn(async move {
            lifecycle_task
                .quiesce_with_deadline(Duration::from_secs(5))
                .await
        });

        wait_for_phase(&lifecycle, AppLifecyclePhase::DrainingRequests).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            waiter.await.expect("timed waiter joins"),
            Err(LifecycleQuiesceError)
        );
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::DrainingRequests);

        drop(request);
        lifecycle
            .quiesce_with_deadline(Duration::from_secs(5))
            .await
            .expect("owned coordinator completes after request release");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
    }

    #[tokio::test(start_paused = true)]
    async fn held_producer_times_out_without_canceling_later_quiescence() {
        let lifecycle = AppLifecycle::new();
        let producer = lifecycle.try_claim_producer().expect("claim producer");
        let lifecycle_task = lifecycle.clone();
        let waiter = tokio::spawn(async move {
            lifecycle_task
                .quiesce_with_deadline(Duration::from_secs(5))
                .await
        });

        wait_for_phase(&lifecycle, AppLifecyclePhase::QuiescingProducers).await;
        tokio::time::advance(Duration::from_secs(5)).await;
        assert_eq!(
            waiter.await.expect("timed waiter joins"),
            Err(LifecycleQuiesceError)
        );
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::QuiescingProducers);

        drop(producer);
        lifecycle
            .quiesce_with_deadline(Duration::from_secs(5))
            .await
            .expect("owned coordinator completes after producer release");
        assert_eq!(lifecycle.phase(), AppLifecyclePhase::Quiesced);
    }

    #[test]
    fn admission_error_is_static_and_bounded() {
        let message = LifecycleAdmissionError.to_string();
        assert_eq!(message, "application shutdown is in progress");
        assert!(message.len() <= 64);
    }

    #[test]
    fn quiesce_error_is_static_and_bounded() {
        let message = LifecycleQuiesceError.to_string();
        assert_eq!(message, "application shutdown quiescence is incomplete");
        assert!(message.len() <= 64);
    }

    async fn wait_for_phase(lifecycle: &AppLifecycle, phase: AppLifecyclePhase) {
        tokio::time::timeout(Duration::from_secs(1), lifecycle.wait_for_phase(phase))
            .await
            .expect("lifecycle phase deadline");
    }
}
