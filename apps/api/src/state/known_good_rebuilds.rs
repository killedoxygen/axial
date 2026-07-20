use super::{
    AppState, IntegrityForegroundLease, ProducerLease, is_canonical_instance_id, known_good,
};
use axial_minecraft::KnownGoodReconstructionError;
use axial_minecraft::known_good::KnownGoodReconstructionReceipt;
use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::sync::{Semaphore, watch};

const MAX_KNOWN_GOOD_REBUILD_FLIGHTS: usize = 1_024;
const MAX_KNOWN_GOOD_REBUILD_OWNERS: usize = 2;
const FLIGHT_LOCK_INVARIANT: &str =
    "known-good rebuild flight lock poisoned; source ownership may be inconsistent";

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum KnownGoodRebuildError {
    #[error("known-good rebuild instance identity is invalid")]
    InvalidInstanceIdentity,
    #[error("known-good rebuild instance is not registered")]
    InstanceNotRegistered,
    #[error("known-good rebuild library root is unavailable")]
    LibraryRootUnavailable,
    #[error("known-good rebuild flight capacity is exhausted")]
    CapacityExhausted,
    #[error("known-good reconstruction failed")]
    ReconstructionFailed,
    #[error("known-good reconstruction returned the wrong identity")]
    ReceiptIdentityMismatch,
    #[error("known-good rebuild target changed")]
    TargetChanged,
    #[error("known-good rebuild did not activate live authority")]
    LiveAuthorityMissing,
    #[error("known-good rebuild owner stopped")]
    OwnerStopped,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct KnownGoodRebuildKey {
    version_id: String,
    library_root: PathBuf,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegisteredKnownGoodRebuildTarget {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
}

impl RegisteredKnownGoodRebuildTarget {
    fn key(&self) -> KnownGoodRebuildKey {
        KnownGoodRebuildKey {
            version_id: self.version_id.clone(),
            library_root: self.library_root.clone(),
        }
    }

    fn matches(
        &self,
        instance: Option<&axial_config::Instance>,
        library_root: Option<&Path>,
    ) -> bool {
        instance.is_some_and(|instance| {
            instance.id == self.instance_id
                && instance.version_id == self.version_id
                && instance.created_at == self.created_at
                && is_canonical_instance_id(&instance.id)
        }) && library_root == Some(self.library_root.as_path())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FlightCompletion {
    ActivationAttempted,
    SourceFailed(KnownGoodRebuildError),
    OwnerStopped,
}

struct InFlightRebuild {
    flight_id: u64,
    completed: watch::Sender<Option<FlightCompletion>>,
}

#[derive(Default)]
struct FlightState {
    next_flight_id: u64,
    in_flight: HashMap<KnownGoodRebuildKey, InFlightRebuild>,
}

pub(super) struct KnownGoodRebuildFlights {
    state: Mutex<FlightState>,
    owner_slots: Arc<Semaphore>,
}

impl Default for KnownGoodRebuildFlights {
    fn default() -> Self {
        Self {
            state: Mutex::new(FlightState::default()),
            owner_slots: Arc::new(Semaphore::new(MAX_KNOWN_GOOD_REBUILD_OWNERS)),
        }
    }
}

enum FlightClaim {
    Own(FlightOwner),
    Wait(FlightWaiter),
}

struct FlightOwner {
    flights: Arc<KnownGoodRebuildFlights>,
    key: KnownGoodRebuildKey,
    flight_id: u64,
    completed: watch::Sender<Option<FlightCompletion>>,
    armed: bool,
}

struct FlightWaiter {
    completed: watch::Receiver<Option<FlightCompletion>>,
}

impl KnownGoodRebuildFlights {
    fn claim(
        self: &Arc<Self>,
        key: KnownGoodRebuildKey,
    ) -> Result<FlightClaim, KnownGoodRebuildError> {
        let mut state = self.state.lock().expect(FLIGHT_LOCK_INVARIANT);
        if let Some(flight) = state.in_flight.get(&key) {
            return Ok(FlightClaim::Wait(FlightWaiter {
                completed: flight.completed.subscribe(),
            }));
        }
        if state.in_flight.len() >= MAX_KNOWN_GOOD_REBUILD_FLIGHTS {
            return Err(KnownGoodRebuildError::CapacityExhausted);
        }
        state.next_flight_id = state
            .next_flight_id
            .checked_add(1)
            .expect("known-good rebuild flight id overflowed");
        let flight_id = state.next_flight_id;
        let (completed, _) = watch::channel(None);
        state.in_flight.insert(
            key.clone(),
            InFlightRebuild {
                flight_id,
                completed: completed.clone(),
            },
        );
        Ok(FlightClaim::Own(FlightOwner {
            flights: self.clone(),
            key,
            flight_id,
            completed,
            armed: true,
        }))
    }

    fn remove_exact(&self, key: &KnownGoodRebuildKey, flight_id: u64) -> bool {
        let mut state = self.state.lock().expect(FLIGHT_LOCK_INVARIANT);
        if state
            .in_flight
            .get(key)
            .is_some_and(|flight| flight.flight_id == flight_id)
        {
            state.in_flight.remove(key);
            true
        } else {
            false
        }
    }
}

impl FlightOwner {
    async fn acquire_slot(
        &self,
    ) -> Result<tokio::sync::OwnedSemaphorePermit, KnownGoodRebuildError> {
        self.flights
            .owner_slots
            .clone()
            .acquire_owned()
            .await
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)
    }

    fn finish(&mut self, completion: FlightCompletion) {
        assert!(
            self.flights.remove_exact(&self.key, self.flight_id),
            "known-good rebuild completion lost exact flight ownership"
        );
        self.completed.send_replace(Some(completion));
        self.armed = false;
    }
}

impl Drop for FlightOwner {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        {
            let mut state = self
                .flights
                .state
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if state
                .in_flight
                .get(&self.key)
                .is_some_and(|flight| flight.flight_id == self.flight_id)
            {
                state.in_flight.remove(&self.key);
            }
        }
        self.completed
            .send_replace(Some(FlightCompletion::OwnerStopped));
    }
}

impl FlightWaiter {
    async fn wait(mut self) -> FlightCompletion {
        loop {
            if let Some(completion) = *self.completed.borrow_and_update() {
                return completion;
            }
            if self.completed.changed().await.is_err() {
                return FlightCompletion::OwnerStopped;
            }
        }
    }
}

impl AppState {
    pub(crate) async fn registered_instance_has_live_known_good(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
    ) -> Result<bool, KnownGoodRebuildError> {
        self.capture_known_good_rebuild_target(foreground, instance_id)
            .await
            .map(|(_, live_authority)| live_authority)
    }

    pub(crate) async fn rebuild_known_good_for_registered_instance<Reconstruct, ReconstructFuture>(
        &self,
        foreground: &IntegrityForegroundLease,
        producer: &ProducerLease,
        instance_id: &str,
        reconstruct: Reconstruct,
    ) -> Result<(), KnownGoodRebuildError>
    where
        Reconstruct: FnOnce(String) -> ReconstructFuture + Send + 'static,
        ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
            + Send
            + 'static,
    {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        let _operation_foreground = foreground.retained();
        let (target, live_authority) = self
            .capture_known_good_rebuild_target(foreground, instance_id)
            .await?;
        if live_authority {
            return Ok(());
        }

        let mut reconstruct = Some(reconstruct);
        let mut missed_fanout_retry = false;
        loop {
            let completion = match self.known_good_rebuilds.claim(target.key())? {
                FlightClaim::Wait(waiter) => waiter.wait().await,
                FlightClaim::Own(owner) => {
                    let reconstruct = reconstruct
                        .take()
                        .expect("known-good rebuild owner lost its source closure");
                    let owner_state = self.clone();
                    let owner_target = target.clone();
                    let owner_foreground = foreground.retained();
                    let owner_task = producer.claim_child().spawn_joinable(async move {
                        let mut owner = owner;
                        let completion = match owner.acquire_slot().await {
                            Ok(permit) => {
                                let reconstruction =
                                    reconstruct(owner_target.version_id.clone()).await;
                                drop(permit);
                                match reconstruction {
                                    Ok(receipt)
                                        if receipt.version_id() == owner_target.version_id =>
                                    {
                                        let _activation_attempt = owner_state
                                            .activate_known_good_source(
                                                &owner_foreground,
                                                &owner_target.library_root,
                                                receipt.into_activation_source(),
                                            )
                                            .await;
                                        FlightCompletion::ActivationAttempted
                                    }
                                    Ok(_) => FlightCompletion::SourceFailed(
                                        KnownGoodRebuildError::ReceiptIdentityMismatch,
                                    ),
                                    Err(_) => FlightCompletion::SourceFailed(
                                        KnownGoodRebuildError::ReconstructionFailed,
                                    ),
                                }
                            }
                            Err(error) => FlightCompletion::SourceFailed(error),
                        };
                        owner.finish(completion);
                        completion
                    });
                    owner_task.await.unwrap_or(FlightCompletion::OwnerStopped)
                }
            };

            match completion {
                FlightCompletion::SourceFailed(error) => return Err(error),
                FlightCompletion::OwnerStopped => {
                    return Err(KnownGoodRebuildError::OwnerStopped);
                }
                FlightCompletion::ActivationAttempted => {
                    match self
                        .postcheck_known_good_rebuild_target(foreground, &target)
                        .await
                    {
                        Ok(()) => return Ok(()),
                        Err(KnownGoodRebuildError::LiveAuthorityMissing)
                            if reconstruct.is_some() && !missed_fanout_retry =>
                        {
                            missed_fanout_retry = true;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    async fn capture_known_good_rebuild_target(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
    ) -> Result<(RegisteredKnownGoodRebuildTarget, bool), KnownGoodRebuildError> {
        if !is_canonical_instance_id(instance_id) {
            return Err(KnownGoodRebuildError::InvalidInstanceIdentity);
        }
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, instance_id)
            .await
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        let instance = self
            .instances
            .get(instance_id)
            .filter(|instance| instance.id == instance_id && is_canonical_instance_id(&instance.id))
            .ok_or(KnownGoodRebuildError::InstanceNotRegistered)?;
        let library_root = self.current_known_good_library_root()?;
        let target = RegisteredKnownGoodRebuildTarget {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
        };
        let live_authority = self
            .known_good
            .active_inventory(
                &target.instance_id,
                &target.version_id,
                &target.created_at,
                &target.library_root,
            )
            .is_some();
        Ok((target, live_authority))
    }

    async fn postcheck_known_good_rebuild_target(
        &self,
        foreground: &IntegrityForegroundLease,
        target: &RegisteredKnownGoodRebuildTarget,
    ) -> Result<(), KnownGoodRebuildError> {
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &target.instance_id)
            .await
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        let current_root = self.current_known_good_library_root().ok();
        let current_instance = self.instances.get(&target.instance_id);
        if !target.matches(current_instance.as_ref(), current_root.as_deref()) {
            self.deactivate_known_good_rebuild_target(target);
            return Err(KnownGoodRebuildError::TargetChanged);
        }
        if self
            .known_good
            .active_inventory(
                &target.instance_id,
                &target.version_id,
                &target.created_at,
                &target.library_root,
            )
            .is_none()
        {
            self.deactivate_known_good_rebuild_target(target);
            return Err(KnownGoodRebuildError::LiveAuthorityMissing);
        }
        Ok(())
    }

    fn deactivate_known_good_rebuild_target(&self, target: &RegisteredKnownGoodRebuildTarget) {
        self.known_good.deactivate_exact(
            &target.instance_id,
            &target.version_id,
            &target.created_at,
            &target.library_root,
        );
    }

    fn current_known_good_library_root(&self) -> Result<PathBuf, KnownGoodRebuildError> {
        let root = self
            .library_dir()
            .map(PathBuf::from)
            .ok_or(KnownGoodRebuildError::LibraryRootUnavailable)?;
        known_good::normalize_library_root(&root)
            .map_err(|_| KnownGoodRebuildError::LibraryRootUnavailable)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppStateInit, InstallStore, KnownGoodVerificationUnavailable, SessionStore,
    };
    use axial_minecraft::known_good::{
        KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity,
        TestKnownGoodRoot,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{Notify, mpsc, oneshot};
    use tokio::time::{Duration, timeout};

    fn test_key(index: usize) -> KnownGoodRebuildKey {
        KnownGoodRebuildKey {
            version_id: format!("version-{index}"),
            library_root: PathBuf::from(format!("/normalized/library/{index}")),
        }
    }

    fn expect_owner(claim: FlightClaim) -> FlightOwner {
        match claim {
            FlightClaim::Own(owner) => owner,
            FlightClaim::Wait(_) => panic!("expected exact flight owner"),
        }
    }

    fn expect_waiter(claim: FlightClaim) -> FlightWaiter {
        match claim {
            FlightClaim::Wait(waiter) => waiter,
            FlightClaim::Own(_) => panic!("expected same-key flight waiter"),
        }
    }

    #[tokio::test]
    async fn same_key_waiters_share_one_completion_and_no_ready_cache() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(1);
        let mut owner = expect_owner(flights.claim(key.clone()).expect("claim owner"));
        let waiters = (0..32)
            .map(|_| expect_waiter(flights.claim(key.clone()).expect("claim waiter")))
            .collect::<Vec<_>>();

        owner.finish(FlightCompletion::ActivationAttempted);
        for waiter in waiters {
            assert_eq!(waiter.wait().await, FlightCompletion::ActivationAttempted);
        }

        let mut retry = expect_owner(
            flights
                .claim(key)
                .expect("completion must not leave a ready cache"),
        );
        retry.finish(FlightCompletion::ActivationAttempted);
    }

    #[test]
    fn exact_key_distinguishes_version_and_normalized_root() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let first_key = test_key(1_000);
        let mut changed_version = first_key.clone();
        changed_version.version_id.push_str("-other");
        let mut changed_root = first_key.clone();
        changed_root.library_root.push("other");

        let first = expect_owner(flights.claim(first_key).expect("first key"));
        let changed_version =
            expect_owner(flights.claim(changed_version).expect("changed version key"));
        let changed_root = expect_owner(flights.claim(changed_root).expect("changed root key"));
        drop((first, changed_version, changed_root));
    }

    #[tokio::test]
    async fn completion_removes_before_wake_and_old_drop_cannot_remove_retry() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(2);
        let mut first = expect_owner(flights.claim(key.clone()).expect("first owner"));
        let first_waiter = expect_waiter(flights.claim(key.clone()).expect("first waiter"));

        first.finish(FlightCompletion::ActivationAttempted);
        let mut retry = expect_owner(
            flights
                .claim(key.clone())
                .expect("retry claims before old waiter wakes"),
        );
        assert_eq!(
            first_waiter.wait().await,
            FlightCompletion::ActivationAttempted
        );
        drop(first);
        let retry_waiter = expect_waiter(
            flights
                .claim(key)
                .expect("old owner cannot remove retry flight"),
        );
        retry.finish(FlightCompletion::ActivationAttempted);
        assert_eq!(
            retry_waiter.wait().await,
            FlightCompletion::ActivationAttempted
        );
    }

    #[tokio::test]
    async fn late_follower_retains_source_closure_for_one_post_fanout_flight() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(3);
        let mut first = expect_owner(flights.claim(key.clone()).expect("first owner"));
        let late = expect_waiter(flights.claim(key.clone()).expect("late follower"));
        let source_calls = Arc::new(AtomicUsize::new(0));
        let retained_source = {
            let source_calls = source_calls.clone();
            move || {
                source_calls.fetch_add(1, Ordering::SeqCst);
            }
        };

        first.finish(FlightCompletion::ActivationAttempted);
        assert_eq!(late.wait().await, FlightCompletion::ActivationAttempted);
        assert_eq!(source_calls.load(Ordering::SeqCst), 0);
        let mut retry = expect_owner(
            flights
                .claim(key)
                .expect("missed target claims one fresh flight"),
        );
        retained_source();
        retry.finish(FlightCompletion::ActivationAttempted);
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn distinct_keys_run_only_two_source_owners_concurrently() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let (started_tx, mut started_rx) = mpsc::unbounded_channel();
        let mut releases = Vec::new();
        let mut tasks = Vec::new();

        for index in 0..3 {
            let (release_tx, release_rx) = oneshot::channel();
            releases.push(Some(release_tx));
            let started_tx = started_tx.clone();
            let mut owner = expect_owner(
                flights
                    .claim(test_key(index + 10))
                    .expect("distinct owner claim"),
            );
            tasks.push(tokio::spawn(async move {
                let permit = owner.acquire_slot().await.expect("source owner slot");
                started_tx.send(index).expect("record source owner");
                let _ = release_rx.await;
                drop(permit);
                owner.finish(FlightCompletion::ActivationAttempted);
            }));
        }
        drop(started_tx);

        let first = timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("first source owner")
            .expect("first source owner id");
        let second = timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("second source owner")
            .expect("second source owner id");
        assert_ne!(first, second);
        assert!(started_rx.try_recv().is_err(), "third owner must wait");

        releases[first]
            .take()
            .expect("first release")
            .send(())
            .expect("release first owner");
        let third = timeout(Duration::from_secs(5), started_rx.recv())
            .await
            .expect("third source owner")
            .expect("third source owner id");
        assert_ne!(third, first);
        assert_ne!(third, second);

        for release in releases.into_iter().flatten() {
            let _ = release.send(());
        }
        for task in tasks {
            task.await.expect("source owner task");
        }
    }

    #[test]
    fn flight_cap_rejects_only_new_keys_without_fallback() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let mut owners = Vec::with_capacity(MAX_KNOWN_GOOD_REBUILD_FLIGHTS);
        for index in 0..MAX_KNOWN_GOOD_REBUILD_FLIGHTS {
            owners.push(expect_owner(
                flights.claim(test_key(index)).expect("bounded owner"),
            ));
        }
        assert!(matches!(
            flights.claim(test_key(MAX_KNOWN_GOOD_REBUILD_FLIGHTS)),
            Err(KnownGoodRebuildError::CapacityExhausted)
        ));
        assert!(matches!(
            flights.claim(test_key(0)),
            Ok(FlightClaim::Wait(_))
        ));
        drop(owners);
    }

    #[tokio::test]
    async fn source_failure_is_fanned_out_but_a_later_call_retries_fresh() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(30);
        let calls = Arc::new(AtomicUsize::new(0));
        let mut first = expect_owner(flights.claim(key.clone()).expect("failed owner"));
        let waiter = expect_waiter(flights.claim(key.clone()).expect("failed waiter"));
        calls.fetch_add(1, Ordering::SeqCst);
        first.finish(FlightCompletion::SourceFailed(
            KnownGoodRebuildError::ReconstructionFailed,
        ));
        assert_eq!(
            waiter.wait().await,
            FlightCompletion::SourceFailed(KnownGoodRebuildError::ReconstructionFailed)
        );
        let second_calls = calls.clone();
        let mut retry = expect_owner(flights.claim(key).expect("fresh retry owner"));
        let permit = retry.acquire_slot().await.expect("fresh retry owner slot");
        second_calls.fetch_add(1, Ordering::SeqCst);
        drop(permit);
        retry.finish(FlightCompletion::ActivationAttempted);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn dropping_an_owner_removes_the_flight_and_wakes_waiters() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(40);
        let owner = expect_owner(flights.claim(key.clone()).expect("owner"));
        let waiter = expect_waiter(flights.claim(key.clone()).expect("waiter"));
        drop(owner);
        assert_eq!(waiter.wait().await, FlightCompletion::OwnerStopped);
        let mut retry = expect_owner(flights.claim(key).expect("retry after owner stop"));
        retry.finish(FlightCompletion::ActivationAttempted);
    }

    #[tokio::test]
    async fn cancelling_owner_task_wakes_follower_and_allows_exact_retry() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let key = test_key(41);
        let started = Arc::new(Notify::new());
        let owner = expect_owner(flights.claim(key.clone()).expect("owner"));
        let owner_started = started.clone();
        let owner_task = tokio::spawn(async move {
            let _permit = owner.acquire_slot().await.expect("source owner slot");
            owner_started.notify_one();
            std::future::pending::<()>().await;
        });
        timeout(Duration::from_secs(5), started.notified())
            .await
            .expect("owner started");
        let waiter = expect_waiter(flights.claim(key.clone()).expect("follower"));

        owner_task.abort();
        assert!(
            owner_task
                .await
                .expect_err("owner cancellation")
                .is_cancelled()
        );
        assert_eq!(
            timeout(Duration::from_secs(5), waiter.wait())
                .await
                .expect("follower wakes"),
            FlightCompletion::OwnerStopped
        );
        let mut retry = expect_owner(flights.claim(key).expect("retry owner"));
        let permit = retry.acquire_slot().await.expect("retry owner slot");
        drop(permit);
        retry.finish(FlightCompletion::ActivationAttempted);
    }

    #[tokio::test]
    async fn cancelling_owner_queued_for_a_source_slot_wakes_its_followers() {
        let flights = Arc::new(KnownGoodRebuildFlights::default());
        let first_owner = expect_owner(flights.claim(test_key(50)).expect("first owner"));
        let second_owner = expect_owner(flights.claim(test_key(51)).expect("second owner"));
        let first_permit = first_owner.acquire_slot().await.expect("first source slot");
        let second_permit = second_owner
            .acquire_slot()
            .await
            .expect("second source slot");

        let queued_key = test_key(52);
        let queued_owner = expect_owner(
            flights
                .claim(queued_key.clone())
                .expect("queued source owner"),
        );
        let queued_waiter = expect_waiter(
            flights
                .claim(queued_key.clone())
                .expect("queued source follower"),
        );
        let acquire_started = Arc::new(Notify::new());
        let task_acquire_started = acquire_started.clone();
        let queued_task = tokio::spawn(async move {
            task_acquire_started.notify_one();
            let _permit = queued_owner
                .acquire_slot()
                .await
                .expect("queued source slot");
            std::future::pending::<()>().await;
        });
        timeout(Duration::from_secs(5), acquire_started.notified())
            .await
            .expect("queued owner reached source slot acquisition");
        assert!(
            !queued_task.is_finished(),
            "the third owner must remain queued"
        );

        queued_task.abort();
        assert!(
            queued_task
                .await
                .expect_err("queued owner cancellation")
                .is_cancelled()
        );
        assert_eq!(
            timeout(Duration::from_secs(5), queued_waiter.wait())
                .await
                .expect("queued follower wakes"),
            FlightCompletion::OwnerStopped
        );
        let mut retry = expect_owner(flights.claim(queued_key).expect("queued-key retry"));

        drop((first_permit, second_permit));
        let retry_permit = retry.acquire_slot().await.expect("retry source slot");
        drop(retry_permit);
        retry.finish(FlightCompletion::ActivationAttempted);
        drop((first_owner, second_owner));
    }

    fn state_fixture(label: &str) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-rebuild-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let config_dir = root.join("config");
        let paths = axial_config::AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        };
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone()).expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                axial_config::InstanceRegistrySnapshot::default(),
            )
            .expect("load test instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                axial_performance::PerformanceManager::load_for_startup(&paths.config_dir)
                    .expect("load test performance state"),
            ),
            startup_warnings: Vec::new(),
        });
        let library_root = root.join("library");
        std::fs::create_dir_all(&library_root).expect("library root");
        state.set_library_dir_for_test(library_root.to_string_lossy().into_owned());
        (state, root)
    }

    async fn close_fixture(state: AppState, root: PathBuf) {
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close instance registry");
        drop(state);
        let _ = std::fs::remove_dir_all(root);
    }

    async fn foreground(state: &AppState) -> IntegrityForegroundLease {
        state
            .register_integrity_foreground()
            .expect("register known-good rebuild foreground")
            .wait_for_settlement()
            .await
    }

    #[tokio::test]
    async fn cancelled_winning_caller_does_not_stop_the_owned_source_or_live_activation() {
        let (state, root) = state_fixture("cancelled-winning-caller");
        let instance = state
            .instances()
            .insert_for_test("Cancellation", "1.21.5")
            .expect("registered instance");
        let source_calls = Arc::new(AtomicUsize::new(0));
        let (source_entered_tx, source_entered_rx) = oneshot::channel();
        let (source_release_tx, source_release_rx) = oneshot::channel();
        let (activation_tx, activation_rx) = oneshot::channel();
        let producer = state.try_claim_producer().expect("claim rebuild producer");
        let operation_foreground = foreground(&state).await;
        let (target, live_authority) = state
            .capture_known_good_rebuild_target(&operation_foreground, &instance.id)
            .await
            .expect("capture cancellation target");
        assert!(!live_authority);

        let caller_state = state.clone();
        let source_state = state.clone();
        let caller_instance_id = instance.id.clone();
        let source_instance_id = instance.id.clone();
        let source_version_id = instance.version_id.clone();
        let caller_producer = producer.claim_child();
        let caller_calls = source_calls.clone();
        let caller = tokio::spawn(async move {
            caller_state
                .rebuild_known_good_for_registered_instance(
                    &operation_foreground,
                    &caller_producer,
                    &caller_instance_id,
                    move |_| async move {
                        caller_calls.fetch_add(1, Ordering::SeqCst);
                        let _ = source_entered_tx.send(());
                        source_release_rx.await.expect("release owned source");
                        source_state.activate_known_good_inventory_for_test(
                            &source_instance_id,
                            verification_test_inventory(&source_version_id),
                        );
                        let _ = activation_tx.send(());
                        Err::<KnownGoodReconstructionReceipt, _>(
                            KnownGoodReconstructionError::Vanilla,
                        )
                    },
                )
                .await
        });

        timeout(Duration::from_secs(5), source_entered_rx)
            .await
            .expect("owned source enters")
            .expect("owned source entry signal");
        let waiter = expect_waiter(
            state
                .known_good_rebuilds
                .claim(target.key())
                .expect("same-key waiter"),
        );
        caller.abort();
        assert!(
            caller
                .await
                .expect_err("cancel winning caller")
                .is_cancelled()
        );
        drop(waiter);
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "the detached owner must retain foreground authority"
        );

        source_release_tx.send(()).expect("release owned source");
        timeout(Duration::from_secs(5), activation_rx)
            .await
            .expect("owned activation completes")
            .expect("owned activation signal");
        let later_foreground = foreground(&state).await;
        let later_calls = source_calls.clone();
        assert_eq!(
            state
                .rebuild_known_good_for_registered_instance(
                    &later_foreground,
                    &producer,
                    &instance.id,
                    move |_| async move {
                        later_calls.fetch_add(1, Ordering::SeqCst);
                        Err::<KnownGoodReconstructionReceipt, _>(
                            KnownGoodReconstructionError::Vanilla,
                        )
                    },
                )
                .await,
            Ok(())
        );
        assert_eq!(source_calls.load(Ordering::SeqCst), 1);

        drop(later_foreground);
        drop(producer);
        state.quiesce().await.expect("owned source drains");
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn capture_binds_canonical_registration_and_normalized_root() {
        let (state, root) = state_fixture("capture");
        let instance = state
            .instances()
            .insert_for_test("Capture", "1.21.5")
            .expect("registered instance");
        let foreground = foreground(&state).await;
        let (target, live_authority) = state
            .capture_known_good_rebuild_target(&foreground, &instance.id)
            .await
            .expect("capture target");
        assert!(!live_authority);
        assert_eq!(target.instance_id, instance.id);
        assert_eq!(target.version_id, instance.version_id);
        assert_eq!(target.created_at, instance.created_at);
        assert_eq!(
            target.library_root,
            std::fs::canonicalize(root.join("library")).expect("canonical library root")
        );
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &target)
                .await,
            Err(KnownGoodRebuildError::LiveAuthorityMissing)
        );
        assert_eq!(
            state
                .capture_known_good_rebuild_target(&foreground, "not-canonical")
                .await,
            Err(KnownGoodRebuildError::InvalidInstanceIdentity)
        );
        close_fixture(state, root).await;
    }

    fn verification_test_inventory(version_id: &str) -> KnownGoodInventory {
        KnownGoodInventory::from_test_entries([TestKnownGoodEntry {
            root: TestKnownGoodRoot::Versions,
            path: format!("{version_id}/{version_id}.jar"),
            kind: KnownGoodArtifactKind::ClientJar,
            integrity: TestKnownGoodIntegrity::File { size: 10 },
        }])
        .expect("verification inventory")
    }

    #[tokio::test]
    async fn verification_lease_binds_exact_live_incarnation_and_current_root() {
        let (state, root) = state_fixture("verification-lease");
        let instance = state
            .instances()
            .insert_for_test("Lease", "1.21.5")
            .expect("registered instance");
        state.activate_known_good_inventory_for_test(
            &instance.id,
            verification_test_inventory(&instance.version_id),
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register verification foreground")
            .wait_for_settlement()
            .await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&foreground, &lifecycle, &root.join("library"))
            .expect("exact live lease");
        let normalized_root = std::fs::canonicalize(root.join("library")).expect("library root");
        assert_eq!(
            lease.exact_identity_for_test(),
            (
                instance.id.as_str(),
                instance.version_id.as_str(),
                instance.created_at.as_str(),
                normalized_root.as_path(),
            )
        );
        drop(lease);

        let different_expected_root = root.join("different-expected-library");
        std::fs::create_dir_all(&different_expected_root).expect("different expected root");
        assert!(matches!(
            state.mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &different_expected_root,
            ),
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));

        let mut recreated = instance.clone();
        recreated.created_at = (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
        state
            .instances()
            .replace_for_test(recreated)
            .expect("replace incarnation");
        assert!(matches!(
            state.mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &root.join("library"),
            ),
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));

        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&changed_root).expect("changed root");
        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        assert!(matches!(
            state.mint_known_good_verification_lease(
                &foreground,
                &lifecycle,
                &root.join("library"),
            ),
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));
        drop(lifecycle);
        drop(foreground);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn version_and_root_drift_fail_exact_lifecycle_postcheck() {
        let (state, root) = state_fixture("version-root-drift");
        let mut instance = state
            .instances()
            .insert_for_test("Drift", "1.21.5")
            .expect("registered instance");
        let foreground = foreground(&state).await;
        let (version_target, _) = state
            .capture_known_good_rebuild_target(&foreground, &instance.id)
            .await
            .expect("version target");
        instance.version_id = "1.21.6".to_string();
        state
            .instances()
            .replace_for_test(instance.clone())
            .expect("replace version");
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &version_target)
                .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );

        let (root_target, _) = state
            .capture_known_good_rebuild_target(&foreground, &instance.id)
            .await
            .expect("root target");
        let changed_root = root.join("changed-library");
        std::fs::create_dir_all(&changed_root).expect("changed root");
        state.set_library_dir_for_test(changed_root.to_string_lossy().into_owned());
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &root_target)
                .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn deletion_and_same_id_recreation_fail_registration_postcheck() {
        let (state, root) = state_fixture("delete-recreate");
        let instance = state
            .instances()
            .insert_for_test("Delete", "1.21.5")
            .expect("registered instance");
        let foreground = foreground(&state).await;
        let (deleted_target, _) = state
            .capture_known_good_rebuild_target(&foreground, &instance.id)
            .await
            .expect("deleted target");
        state
            .instances()
            .remove_for_test(&instance.id)
            .expect("delete registration");
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &deleted_target)
                .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );

        let recreated = state
            .instances()
            .insert_for_test("Recreated", "1.21.5")
            .expect("recreated registration");
        let (recreated_target, _) = state
            .capture_known_good_rebuild_target(&foreground, &recreated.id)
            .await
            .expect("recreated target");
        let mut replacement = recreated.clone();
        replacement.created_at = (chrono::Utc::now() + chrono::Duration::seconds(1)).to_rfc3339();
        state
            .instances()
            .replace_for_test(replacement)
            .expect("same-id replacement");
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &recreated_target)
                .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn persisted_and_installed_evidence_never_suppresses_fresh_source_work() {
        let (state, root) = state_fixture("evidence-non-authority");
        let instance = state
            .instances()
            .insert_for_test("Evidence", "1.21.5")
            .expect("registered instance");
        let version_dir = root.join("library/versions/1.21.5");
        std::fs::create_dir_all(&version_dir).expect("installed version directory");
        std::fs::write(version_dir.join("1.21.5.json"), b"installed-json")
            .expect("installed metadata");
        std::fs::write(version_dir.join("1.21.5.jar"), b"installed-client")
            .expect("installed client");
        let snapshot_dir = root.join("config/state/known-good");
        std::fs::create_dir_all(&snapshot_dir).expect("snapshot directory");
        std::fs::write(
            snapshot_dir.join(format!("{}.json", instance.id)),
            format!(
                "{{\"schema\":\"axial.state.known_good_inventory.v4\",\"instance_id\":\"{}\",\"version_id\":\"1.21.5\",\"entries\":[{{\"root\":{{\"kind\":\"versions\"}},\"path\":\"1.21.5/1.21.5.json\",\"kind\":\"version_metadata\",\"integrity\":{{\"kind\":\"sha1\",\"digest\":\"0000000000000000000000000000000000000000\",\"size\":1}}}}]}}",
                instance.id
            ),
        )
        .expect("persisted snapshot evidence");

        let calls = Arc::new(AtomicUsize::new(0));
        let foreground = foreground(&state).await;
        let producer = state.try_claim_producer().expect("claim rebuild owner");
        for _ in 0..2 {
            let calls = calls.clone();
            assert_eq!(
                state
                    .rebuild_known_good_for_registered_instance(
                        &foreground,
                        &producer,
                        &instance.id,
                        move |version_id| async move {
                            assert_eq!(version_id, "1.21.5");
                            calls.fetch_add(1, Ordering::SeqCst);
                            Err::<KnownGoodReconstructionReceipt, _>(
                                KnownGoodReconstructionError::Vanilla,
                            )
                        },
                    )
                    .await,
                Err(KnownGoodRebuildError::ReconstructionFailed)
            );
        }
        assert_eq!(calls.load(Ordering::SeqCst), 2);
        let (target, live_authority) = state
            .capture_known_good_rebuild_target(&foreground, &instance.id)
            .await
            .expect("current target");
        assert!(!live_authority);
        assert_eq!(
            state
                .postcheck_known_good_rebuild_target(&foreground, &target)
                .await,
            Err(KnownGoodRebuildError::LiveAuthorityMissing),
            "persisted evidence must not hydrate live authority"
        );
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        assert!(
            matches!(
                state.mint_known_good_verification_lease(
                    &foreground,
                    &lifecycle,
                    &root.join("library"),
                ),
                Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
            ),
            "persisted evidence must not mint verification authority"
        );
        drop(lifecycle);
        drop(foreground);
        close_fixture(state, root).await;
    }
}
