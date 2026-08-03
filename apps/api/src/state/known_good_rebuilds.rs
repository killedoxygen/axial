use super::{
    AppState, IntegrityForegroundLease, LibraryOperation, ProducerLease, is_canonical_instance_id,
    known_good,
};
use axial_minecraft::known_good::KnownGoodReconstructionReceipt;
use axial_minecraft::{
    KnownGoodReconstructionError, ManagedInstallActivationContractId,
    verify_managed_install_reconstruction_checkpoint, verify_registered_known_good_bootstrap,
};
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
    #[error("known-good persisted authority is invalid")]
    PersistedAuthorityInvalid,
    #[error("known-good reconstruction could not be verified")]
    VerificationFailed,
    #[error("known-good verified activation was rejected")]
    ActivationRejected,
    #[error("known-good rebuild target changed")]
    TargetChanged,
    #[error("known-good rebuild is deferred while install recovery restores authority")]
    InstallRecoveryActive,
    #[error("known-good rebuild did not activate live authority")]
    LiveAuthorityMissing,
    #[error("known-good rebuild owner stopped")]
    OwnerStopped,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct KnownGoodRebuildKey {
    version_id: String,
    library_root: PathBuf,
    activation_contract_id: Option<ManagedInstallActivationContractId>,
}

#[derive(Clone)]
struct RegisteredKnownGoodRebuildTarget {
    instance_id: String,
    version_id: String,
    created_at: String,
    library_root: PathBuf,
    library_operation: LibraryOperation,
    activation_contract_id: Option<ManagedInstallActivationContractId>,
}

struct ExpectedRegisteredKnownGoodIncarnation {
    version_id: String,
    created_at: String,
    library_operation: LibraryOperation,
    persisted_contract: Option<ManagedInstallActivationContractId>,
    activation_contract: ManagedInstallActivationContractId,
}

pub(crate) struct RegisteredKnownGoodRebuildIncarnation {
    pub(crate) instance_id: String,
    pub(crate) version_id: String,
    pub(crate) created_at: String,
    pub(crate) persisted_contract: Option<ManagedInstallActivationContractId>,
}

pub(crate) enum RegisteredKnownGoodRebuildSelection {
    NoRegisteredCandidates,
    Eligible(RegisteredKnownGoodRebuildIncarnation),
    RegisteredButIncompatible,
}

impl RegisteredKnownGoodRebuildTarget {
    fn key(&self) -> KnownGoodRebuildKey {
        KnownGoodRebuildKey {
            version_id: self.version_id.clone(),
            library_root: self.library_root.clone(),
            activation_contract_id: self.activation_contract_id.clone(),
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

#[derive(Clone, Debug, Eq, PartialEq)]
enum FlightCompletion {
    Activated(ManagedInstallActivationContractId),
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

    fn finish(&mut self, completion: FlightCompletion) -> FlightCompletion {
        assert!(
            self.flights.remove_exact(&self.key, self.flight_id),
            "known-good rebuild completion lost exact flight ownership"
        );
        self.completed.send_replace(Some(completion.clone()));
        self.armed = false;
        completion
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
            if let Some(completion) = self.completed.borrow_and_update().clone() {
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

    pub(crate) async fn select_registered_known_good_rebuild_incarnation(
        &self,
        foreground: &IntegrityForegroundLease,
        library_operation: &LibraryOperation,
        version_id: &str,
        expected_contract: &ManagedInstallActivationContractId,
    ) -> Result<RegisteredKnownGoodRebuildSelection, KnownGoodRebuildError> {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        self.validate_managed_library_operation(library_operation)
            .map_err(|_| KnownGoodRebuildError::LibraryRootUnavailable)?;
        let mut candidates = self
            .instances
            .list()
            .into_iter()
            .filter(|instance| {
                instance.version_id == version_id && is_canonical_instance_id(&instance.id)
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| left.id.cmp(&right.id));
        if candidates.is_empty() {
            return Ok(RegisteredKnownGoodRebuildSelection::NoRegisteredCandidates);
        }
        let mut absent = None;
        for candidate in candidates {
            let _lifecycle = self
                .acquire_integrity_instance_lifecycle(foreground, &candidate.id)
                .await
                .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
            let Some(current) = self.instances.get(&candidate.id).filter(|current| {
                current.id == candidate.id
                    && current.version_id == candidate.version_id
                    && current.created_at == candidate.created_at
            }) else {
                continue;
            };
            let persisted = match self
                .known_good
                .persisted_activation_contract(&current.id, &current.version_id)
                .await
            {
                Ok(persisted) => persisted,
                Err(error) if error.kind() == std::io::ErrorKind::InvalidData => continue,
                Err(_) => return Err(KnownGoodRebuildError::PersistedAuthorityInvalid),
            };
            self.validate_managed_library_operation(library_operation)
                .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
            let incarnation = RegisteredKnownGoodRebuildIncarnation {
                instance_id: current.id,
                version_id: current.version_id,
                created_at: current.created_at,
                persisted_contract: persisted.clone(),
            };
            match persisted {
                Some(contract) if &contract == expected_contract => {
                    return Ok(RegisteredKnownGoodRebuildSelection::Eligible(incarnation));
                }
                None if absent.is_none() => absent = Some(incarnation),
                Some(_) | None => {}
            }
        }
        self.validate_managed_library_operation(library_operation)
            .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
        Ok(match absent {
            Some(incarnation) => RegisteredKnownGoodRebuildSelection::Eligible(incarnation),
            None => RegisteredKnownGoodRebuildSelection::RegisteredButIncompatible,
        })
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
        self.rebuild_known_good_for_registered_instance_with_expected_incarnation(
            foreground,
            producer,
            instance_id,
            None,
            reconstruct,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn rehydrate_known_good_for_registered_instance<
        Reconstruct,
        ReconstructFuture,
    >(
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
        let (target, live_authority) = self
            .capture_known_good_rebuild_target(foreground, instance_id)
            .await?;
        if live_authority || target.activation_contract_id.is_none() {
            return Ok(());
        }
        let activation_contract = target
            .activation_contract_id
            .clone()
            .ok_or(KnownGoodRebuildError::PersistedAuthorityInvalid)?;
        self.rebuild_known_good_for_registered_instance_with_expected_incarnation(
            foreground,
            producer,
            instance_id,
            Some(ExpectedRegisteredKnownGoodIncarnation {
                version_id: target.version_id,
                created_at: target.created_at,
                library_operation: target.library_operation,
                persisted_contract: Some(activation_contract.clone()),
                activation_contract,
            }),
            reconstruct,
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn rebuild_known_good_for_registered_instance_incarnation<
        Reconstruct,
        ReconstructFuture,
    >(
        &self,
        foreground: &IntegrityForegroundLease,
        producer: &ProducerLease,
        instance_id: &str,
        expected_version_id: &str,
        expected_created_at: &str,
        expected_library_operation: &LibraryOperation,
        expected_persisted_contract: Option<&ManagedInstallActivationContractId>,
        expected_activation_contract: &ManagedInstallActivationContractId,
        reconstruct: Reconstruct,
    ) -> Result<ManagedInstallActivationContractId, KnownGoodRebuildError>
    where
        Reconstruct: FnOnce(String) -> ReconstructFuture + Send + 'static,
        ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
            + Send
            + 'static,
    {
        self.rebuild_known_good_for_registered_instance_with_expected_incarnation(
            foreground,
            producer,
            instance_id,
            Some(ExpectedRegisteredKnownGoodIncarnation {
                version_id: expected_version_id.to_string(),
                created_at: expected_created_at.to_string(),
                library_operation: expected_library_operation.clone(),
                persisted_contract: expected_persisted_contract.cloned(),
                activation_contract: expected_activation_contract.clone(),
            }),
            reconstruct,
        )
        .await
    }

    async fn rebuild_known_good_for_registered_instance_with_expected_incarnation<
        Reconstruct,
        ReconstructFuture,
    >(
        &self,
        foreground: &IntegrityForegroundLease,
        producer: &ProducerLease,
        instance_id: &str,
        expected: Option<ExpectedRegisteredKnownGoodIncarnation>,
        reconstruct: Reconstruct,
    ) -> Result<ManagedInstallActivationContractId, KnownGoodRebuildError>
    where
        Reconstruct: FnOnce(String) -> ReconstructFuture + Send + 'static,
        ReconstructFuture: Future<Output = Result<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>>
            + Send
            + 'static,
    {
        self.validate_integrity_foreground(foreground)
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        let _operation_foreground = foreground.retained();
        let (mut target, live_authority) = self
            .capture_expected_known_good_rebuild_target(foreground, instance_id, expected.as_ref())
            .await?;
        if live_authority {
            return target
                .activation_contract_id
                .clone()
                .ok_or(KnownGoodRebuildError::LiveAuthorityMissing);
        }

        let mut reconstruct = Some(reconstruct);
        let mut missed_fanout_retry = false;
        let mut target_change_retry = false;
        loop {
            let completion = match self.known_good_rebuilds.claim(target.key())? {
                FlightClaim::Wait(waiter) => waiter.wait().await,
                FlightClaim::Own(owner) => {
                    let reconstruct = reconstruct
                        .take()
                        .expect("known-good rebuild owner lost its source closure");
                    let owner_state = self.clone();
                    let owner_target = target.clone();
                    let owner_expected_activation = expected
                        .as_ref()
                        .map(|expected| expected.activation_contract.clone());
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
                                        let current_contract = match owner_state
                                            .known_good
                                            .persisted_activation_contract(
                                                &owner_target.instance_id,
                                                &owner_target.version_id,
                                            )
                                            .await
                                        {
                                            Ok(contract) => contract,
                                            Err(_) => {
                                                return owner.finish(
                                                    FlightCompletion::SourceFailed(
                                                        KnownGoodRebuildError::PersistedAuthorityInvalid,
                                                    ),
                                                );
                                            }
                                        };
                                        if current_contract
                                            != owner_target.activation_contract_id
                                        {
                                            return owner.finish(
                                                FlightCompletion::SourceFailed(
                                                    KnownGoodRebuildError::TargetChanged,
                                                ),
                                            );
                                        }
                                        match owner_target.activation_contract_id.as_ref() {
                                            Some(contract) => {
                                                let verified =
                                                    match verify_managed_install_reconstruction_checkpoint(
                                                        contract,
                                                        receipt,
                                                    ) {
                                                        Ok(verified) => verified,
                                                        Err(_) => {
                                                            return owner.finish(
                                                                FlightCompletion::SourceFailed(
                                                                    KnownGoodRebuildError::VerificationFailed,
                                                                ),
                                                            );
                                                        }
                                                    };
                                                let activation_contract_id =
                                                    verified.activation_contract_id().clone();
                                                if owner_expected_activation.as_ref().is_some_and(
                                                    |expected| {
                                                        expected != &activation_contract_id
                                                    },
                                                ) {
                                                    return owner.finish(
                                                        FlightCompletion::SourceFailed(
                                                            KnownGoodRebuildError::VerificationFailed,
                                                        ),
                                                    );
                                                }
                                                match owner_state
                                                    .accept_verified_registered_known_good_checkpoint(
                                                        &owner_foreground,
                                                        &owner_target.instance_id,
                                                        &owner_target.library_operation,
                                                        verified,
                                                    )
                                                    .await
                                                {
                                                    Ok(()) => FlightCompletion::Activated(
                                                        activation_contract_id,
                                                    ),
                                                    Err(_) => FlightCompletion::SourceFailed(
                                                        KnownGoodRebuildError::ActivationRejected,
                                                    ),
                                                }
                                            }
                                            None => {
                                                let verified =
                                                    match verify_registered_known_good_bootstrap(
                                                        owner_target
                                                            .library_operation
                                                            .retained_core(),
                                                        receipt,
                                                    )
                                                    .await
                                                    {
                                                        Ok(verified) => verified,
                                                        Err(_) => {
                                                            return owner.finish(
                                                                FlightCompletion::SourceFailed(
                                                                    KnownGoodRebuildError::VerificationFailed,
                                                                ),
                                                            );
                                                        }
                                                    };
                                                let activation_contract_id =
                                                    verified.activation_contract_id().clone();
                                                if owner_expected_activation.as_ref().is_some_and(
                                                    |expected| {
                                                        expected != &activation_contract_id
                                                    },
                                                ) {
                                                    return owner.finish(
                                                        FlightCompletion::SourceFailed(
                                                            KnownGoodRebuildError::VerificationFailed,
                                                        ),
                                                    );
                                                }
                                                match owner_state
                                                    .accept_verified_registered_known_good_bootstrap(
                                                        &owner_foreground,
                                                        &owner_target.instance_id,
                                                        &owner_target.library_operation,
                                                        verified,
                                                    )
                                                    .await
                                                {
                                                    Ok(()) => FlightCompletion::Activated(
                                                        activation_contract_id,
                                                    ),
                                                    Err(_) => FlightCompletion::SourceFailed(
                                                        KnownGoodRebuildError::ActivationRejected,
                                                    ),
                                                }
                                            }
                                        }
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
                        owner.finish(completion)
                    });
                    owner_task.await.unwrap_or(FlightCompletion::OwnerStopped)
                }
            };

            match completion {
                FlightCompletion::SourceFailed(KnownGoodRebuildError::TargetChanged)
                    if reconstruct.is_some() && !target_change_retry =>
                {
                    target_change_retry = true;
                    let (current_target, live) = self
                        .capture_expected_known_good_rebuild_target(
                            foreground,
                            instance_id,
                            expected.as_ref(),
                        )
                        .await?;
                    if live {
                        return current_target
                            .activation_contract_id
                            .ok_or(KnownGoodRebuildError::LiveAuthorityMissing);
                    }
                    target = current_target;
                }
                FlightCompletion::SourceFailed(error) => return Err(error),
                FlightCompletion::OwnerStopped => {
                    return Err(KnownGoodRebuildError::OwnerStopped);
                }
                FlightCompletion::Activated(activation_contract_id) => {
                    match self
                        .postcheck_known_good_rebuild_target(
                            foreground,
                            &target,
                            &activation_contract_id,
                        )
                        .await
                    {
                        Ok(()) => return Ok(activation_contract_id),
                        Err(KnownGoodRebuildError::LiveAuthorityMissing)
                            if reconstruct.is_some() && !missed_fanout_retry =>
                        {
                            missed_fanout_retry = true;
                            let (current_target, live) = self
                                .capture_expected_known_good_rebuild_target(
                                    foreground,
                                    instance_id,
                                    expected.as_ref(),
                                )
                                .await?;
                            if live {
                                return current_target
                                    .activation_contract_id
                                    .ok_or(KnownGoodRebuildError::LiveAuthorityMissing);
                            }
                            target = current_target;
                        }
                        Err(error) => return Err(error),
                    }
                }
            }
        }
    }

    async fn capture_expected_known_good_rebuild_target(
        &self,
        foreground: &IntegrityForegroundLease,
        instance_id: &str,
        expected: Option<&ExpectedRegisteredKnownGoodIncarnation>,
    ) -> Result<(RegisteredKnownGoodRebuildTarget, bool), KnownGoodRebuildError> {
        if let Some(expected) = expected {
            self.validate_managed_library_operation(&expected.library_operation)
                .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
        }
        let (target, live_authority) = self
            .capture_known_good_rebuild_target(foreground, instance_id)
            .await?;
        if expected.is_some_and(|expected| {
            target.version_id != expected.version_id
                || target.created_at != expected.created_at
                || target.library_operation.generation() != expected.library_operation.generation()
                || target.activation_contract_id != expected.persisted_contract
        }) {
            return Err(KnownGoodRebuildError::TargetChanged);
        }
        if let Some(expected) = expected {
            self.validate_managed_library_operation(&expected.library_operation)
                .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
        }
        Ok((target, live_authority))
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
        let library_operation = self
            .try_acquire_managed_library()
            .map_err(|_| KnownGoodRebuildError::LibraryRootUnavailable)?;
        let library_root = known_good::normalize_library_root(library_operation.configured_path())
            .map_err(|_| KnownGoodRebuildError::LibraryRootUnavailable)?;
        let activation_contract_id = self
            .known_good
            .persisted_activation_contract(&instance.id, &instance.version_id)
            .await
            .map_err(|_| KnownGoodRebuildError::PersistedAuthorityInvalid)?;
        let target = RegisteredKnownGoodRebuildTarget {
            instance_id: instance.id,
            version_id: instance.version_id,
            created_at: instance.created_at,
            library_root,
            library_operation,
            activation_contract_id,
        };
        let active_source = self.known_good.active_source(
            &target.instance_id,
            &target.version_id,
            &target.created_at,
            &target.library_root,
        );
        let live_authority = match (&target.activation_contract_id, active_source) {
            (Some(expected), Some(source)) => source.activation_contract_id() == expected,
            (Some(_), None) | (None, None) => false,
            (None, Some(_)) => return Err(KnownGoodRebuildError::LiveAuthorityMissing),
        };
        Ok((target, live_authority))
    }

    async fn postcheck_known_good_rebuild_target(
        &self,
        foreground: &IntegrityForegroundLease,
        target: &RegisteredKnownGoodRebuildTarget,
        expected_contract: &ManagedInstallActivationContractId,
    ) -> Result<(), KnownGoodRebuildError> {
        let _lifecycle = self
            .acquire_integrity_instance_lifecycle(foreground, &target.instance_id)
            .await
            .map_err(|_| KnownGoodRebuildError::OwnerStopped)?;
        let current_root = self
            .try_acquire_managed_library()
            .ok()
            .and_then(|operation| {
                known_good::normalize_library_root(operation.configured_path()).ok()
            });
        let current_instance = self.instances.get(&target.instance_id);
        if !target.matches(current_instance.as_ref(), current_root.as_deref()) {
            return Err(KnownGoodRebuildError::TargetChanged);
        }
        target
            .library_operation
            .revalidate()
            .map_err(|_| KnownGoodRebuildError::TargetChanged)?;
        let persisted_contract = self
            .known_good
            .persisted_activation_contract(&target.instance_id, &target.version_id)
            .await
            .map_err(|_| KnownGoodRebuildError::PersistedAuthorityInvalid)?;
        let active_source = self.known_good.active_source(
            &target.instance_id,
            &target.version_id,
            &target.created_at,
            &target.library_root,
        );
        if target
            .activation_contract_id
            .as_ref()
            .is_some_and(|captured| captured != expected_contract)
        {
            return Err(KnownGoodRebuildError::TargetChanged);
        }
        if persisted_contract.as_ref() != Some(expected_contract)
            || active_source
                .as_ref()
                .is_none_or(|source| source.activation_contract_id() != expected_contract)
        {
            return Err(KnownGoodRebuildError::LiveAuthorityMissing);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{
        AppStateInit, InstallStore, KnownGoodVerificationUnavailable, SessionStore,
    };
    use axial_minecraft::known_good::{
        KnownGoodActivationSource, KnownGoodArtifactKind, KnownGoodInventory, TestKnownGoodEntry,
        TestKnownGoodIntegrity, TestKnownGoodRoot,
    };
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::sync::{Notify, mpsc, oneshot};
    use tokio::time::{Duration, timeout};

    fn test_contract() -> ManagedInstallActivationContractId {
        ManagedInstallActivationContractId::parse(
            "managed-install-activation-v1.qqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqqo",
        )
        .expect("canonical test activation contract")
    }

    fn alternate_contract(
        contract: &ManagedInstallActivationContractId,
    ) -> ManagedInstallActivationContractId {
        let mut alternate = contract.to_string();
        let digest_start = alternate.find('.').expect("contract separator") + 1;
        let replacement = if &alternate[digest_start..digest_start + 1] == "r" {
            "s"
        } else {
            "r"
        };
        alternate.replace_range(digest_start..digest_start + 1, replacement);
        ManagedInstallActivationContractId::parse(&alternate)
            .expect("alternate canonical activation contract")
    }

    fn activated_completion() -> FlightCompletion {
        FlightCompletion::Activated(test_contract())
    }

    fn test_key(index: usize) -> KnownGoodRebuildKey {
        KnownGoodRebuildKey {
            version_id: format!("version-{index}"),
            library_root: PathBuf::from(format!("/normalized/library/{index}")),
            activation_contract_id: None,
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

        owner.finish(activated_completion());
        for waiter in waiters {
            assert_eq!(waiter.wait().await, activated_completion());
        }

        let mut retry = expect_owner(
            flights
                .claim(key)
                .expect("completion must not leave a ready cache"),
        );
        retry.finish(activated_completion());
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

        first.finish(activated_completion());
        let mut retry = expect_owner(
            flights
                .claim(key.clone())
                .expect("retry claims before old waiter wakes"),
        );
        assert_eq!(first_waiter.wait().await, activated_completion());
        drop(first);
        let retry_waiter = expect_waiter(
            flights
                .claim(key)
                .expect("old owner cannot remove retry flight"),
        );
        retry.finish(activated_completion());
        assert_eq!(retry_waiter.wait().await, activated_completion());
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

        first.finish(activated_completion());
        assert_eq!(late.wait().await, activated_completion());
        assert_eq!(source_calls.load(Ordering::SeqCst), 0);
        let mut retry = expect_owner(
            flights
                .claim(key)
                .expect("missed target claims one fresh flight"),
        );
        retained_source();
        retry.finish(activated_completion());
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
                owner.finish(activated_completion());
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
        retry.finish(activated_completion());
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
        retry.finish(activated_completion());
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
        retry.finish(activated_completion());
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
        retry.finish(activated_completion());
        drop((first_owner, second_owner));
    }

    async fn state_fixture(label: &str) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-known-good-rebuild-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&root);
        let paths =
            axial_config::AppPaths::from_root(root.to_path_buf()).expect("absolute test app root");
        let root_session = crate::state::test_root_session(&paths);
        let config = Arc::new(
            axial_config::ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                .expect("load test config"),
        );
        let instances = Arc::new(
            axial_config::InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
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
                axial_performance::PerformanceManager::load_for_startup(paths.performance_dir())
                    .expect("load test performance state"),
            ),
            startup_warnings: Vec::new(),
        });
        let foreground = foreground(&state).await;
        let target = state
            .managed_library_setup_target(&foreground)
            .expect("managed library setup target");
        state
            .commit_managed_library_setup(&foreground, &target)
            .await
            .expect("configure managed library");
        (state, root)
    }

    async fn close_fixture(state: AppState, root: PathBuf) {
        state
            .shutdown()
            .await
            .expect("shutdown known-good rebuild fixture state");
        drop(state);
        std::fs::remove_dir_all(root).expect("remove known-good rebuild test root");
    }

    async fn foreground(state: &AppState) -> IntegrityForegroundLease {
        state
            .register_integrity_foreground()
            .expect("register known-good rebuild foreground")
            .wait_for_settlement()
            .await
    }

    #[tokio::test]
    async fn version_scoped_rebuild_rejects_retarget_before_lifecycle_capture() {
        let (state, root) = state_fixture("version-retarget-race").await;
        let instance = state
            .instances()
            .insert_for_test("Retargeted", "1.21.5")
            .expect("registered instance");
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let source_calls = Arc::new(AtomicUsize::new(0));
        let caller_calls = source_calls.clone();
        let caller_state = state.clone();
        let caller_instance_id = instance.id.clone();
        let expected_version_id = instance.version_id.clone();
        let expected_created_at = instance.created_at.clone();
        let caller_foreground = foreground(&state).await;
        let caller_library_operation = state
            .try_acquire_managed_library()
            .expect("capture version-scoped library operation");
        let caller_producer = state
            .try_claim_producer()
            .expect("claim version-scoped rebuild producer");
        let caller = tokio::spawn(async move {
            caller_state
                .rebuild_known_good_for_registered_instance_incarnation(
                    &caller_foreground,
                    &caller_producer,
                    &caller_instance_id,
                    &expected_version_id,
                    &expected_created_at,
                    &caller_library_operation,
                    None,
                    &test_contract(),
                    move |_| async move {
                        caller_calls.fetch_add(1, Ordering::SeqCst);
                        Err(KnownGoodReconstructionError::Vanilla)
                    },
                )
                .await
        });

        tokio::task::yield_now().await;
        let mut replacement = instance;
        replacement.version_id = "1.21.6".to_string();
        state
            .instances()
            .replace_for_test(replacement)
            .expect("retarget registered instance");
        drop(lifecycle);

        assert_eq!(
            timeout(Duration::from_secs(5), caller)
                .await
                .expect("retargeted rebuild returns")
                .expect("retargeted rebuild task"),
            Err(KnownGoodRebuildError::TargetChanged)
        );
        assert_eq!(
            source_calls.load(Ordering::SeqCst),
            0,
            "a retargeted instance must never reconstruct the replacement version"
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn checkpoint_selector_prefers_exact_then_absent_and_rejects_all_incompatible() {
        let (state, root) = state_fixture("checkpoint-selector").await;
        let version_id = "1.21.5";
        let library_operation = state
            .try_acquire_managed_library()
            .expect("capture selector library");
        let foreground = foreground(&state).await;
        let exact_contract = test_contract();
        let mismatch_contract = alternate_contract(&exact_contract);
        let absent_contract = alternate_contract(&mismatch_contract);

        let exact = state
            .instances()
            .insert_for_test("Exact", version_id)
            .expect("register exact peer");
        state
            .activate_known_good_source_before_final_validation(
                &foreground,
                library_operation.configured_path(),
                KnownGoodActivationSource::from_test_inventory(
                    version_id,
                    verification_test_inventory(version_id),
                    exact_contract.clone(),
                )
                .expect("exact source"),
                Some(library_operation.clone()),
                known_good::KnownGoodPersistencePolicy::Install,
                || std::future::ready(Ok(())),
            )
            .await
            .expect("persist exact authority");
        assert!(state.deactivate_registered_known_good_for_test(&exact.id));

        let mut mismatched = state
            .instances()
            .insert_for_test("Mismatch", "1.21.6")
            .expect("register mismatched peer");
        state
            .activate_known_good_source_before_final_validation(
                &foreground,
                library_operation.configured_path(),
                KnownGoodActivationSource::from_test_inventory(
                    "1.21.6",
                    verification_test_inventory("1.21.6"),
                    mismatch_contract,
                )
                .expect("mismatched source"),
                Some(library_operation.clone()),
                known_good::KnownGoodPersistencePolicy::Install,
                || std::future::ready(Ok(())),
            )
            .await
            .expect("persist mismatched authority");
        assert!(state.deactivate_registered_known_good_for_test(&mismatched.id));
        mismatched.version_id = version_id.to_string();
        state
            .instances()
            .replace_for_test(mismatched.clone())
            .expect("retarget mismatched peer");

        let absent = state
            .instances()
            .insert_for_test("Absent", version_id)
            .expect("register absent peer");
        let exact_selection = state
            .select_registered_known_good_rebuild_incarnation(
                &foreground,
                &library_operation,
                version_id,
                &exact_contract,
            )
            .await
            .expect("select exact authority");
        assert!(matches!(
            exact_selection,
            RegisteredKnownGoodRebuildSelection::Eligible(ref selected)
                if selected.instance_id == exact.id
                    && selected.persisted_contract.as_ref() == Some(&exact_contract)
        ));

        let absent_selection = state
            .select_registered_known_good_rebuild_incarnation(
                &foreground,
                &library_operation,
                version_id,
                &absent_contract,
            )
            .await
            .expect("select absent authority");
        assert!(matches!(
            absent_selection,
            RegisteredKnownGoodRebuildSelection::Eligible(ref selected)
                if selected.instance_id == absent.id && selected.persisted_contract.is_none()
        ));

        state
            .instances()
            .remove_for_test(&absent.id)
            .expect("remove absent peer");
        assert!(matches!(
            state
                .select_registered_known_good_rebuild_incarnation(
                    &foreground,
                    &library_operation,
                    version_id,
                    &absent_contract,
                )
                .await
                .expect("classify incompatible cohort"),
            RegisteredKnownGoodRebuildSelection::RegisteredButIncompatible
        ));

        drop((foreground, library_operation));
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn cancelled_winning_caller_does_not_stop_the_owned_source_or_live_activation() {
        let (state, root) = state_fixture("cancelled-winning-caller").await;
        let instance = state
            .instances()
            .insert_for_test("Cancellation", "1.21.5")
            .expect("registered instance");
        let fixture_operation = state
            .try_acquire_managed_library()
            .expect("capture cancellation fixture library");
        let publication = axial_minecraft::publish_managed_install_fixture_for_test(
            fixture_operation.retained_core(),
            &instance.version_id,
        )
        .await
        .expect("publish cancellation fixture");
        let evidence = match axial_minecraft::classify_managed_install_publication(
            fixture_operation.retained_core(),
            instance.version_id.clone(),
        )
        .await
        {
            axial_minecraft::ManagedInstallDurableOutcome::Committed(evidence) => evidence,
            _ => panic!("cancellation fixture must expose committed evidence"),
        };
        let acknowledgement = evidence
            .verify_install_receipt(publication)
            .expect("verify cancellation fixture receipt")
            .activate_with(|_| async { Ok::<(), axial_minecraft::KnownGoodActivationRejected>(()) })
            .await
            .expect("activate cancellation fixture publication");
        assert!(matches!(
            acknowledgement.acknowledge().await,
            axial_minecraft::ManagedInstallAcknowledgementOutcome::Acknowledged
        ));
        drop(fixture_operation);
        let source_calls = Arc::new(AtomicUsize::new(0));
        let (source_entered_tx, source_entered_rx) = oneshot::channel();
        let (source_release_tx, source_release_rx) = oneshot::channel();
        let (source_returned_tx, source_returned_rx) = oneshot::channel();
        let producer = state.try_claim_producer().expect("claim rebuild producer");
        let operation_foreground = foreground(&state).await;
        let (target, live_authority) = state
            .capture_known_good_rebuild_target(&operation_foreground, &instance.id)
            .await
            .expect("capture cancellation target");
        assert!(!live_authority);

        let caller_state = state.clone();
        let caller_instance_id = instance.id.clone();
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
                        let receipt =
                            axial_minecraft::managed_install_reconstruction_receipt_fixture_for_test(
                                &source_version_id,
                            )
                            .map_err(|_| KnownGoodReconstructionError::Vanilla)?;
                        let _ = source_returned_tx.send(());
                        Ok::<KnownGoodReconstructionReceipt, KnownGoodReconstructionError>(receipt)
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
        timeout(Duration::from_secs(5), source_returned_rx)
            .await
            .expect("owned source completes")
            .expect("owned source completion signal");
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

        drop(target);
        drop(later_foreground);
        drop(producer);
        state.quiesce().await.expect("owned source drains");
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn capture_binds_canonical_registration_and_normalized_root() {
        let (state, root) = state_fixture("capture").await;
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
                .postcheck_known_good_rebuild_target(&foreground, &target, &test_contract(),)
                .await,
            Err(KnownGoodRebuildError::LiveAuthorityMissing)
        );
        assert!(matches!(
            state
                .capture_known_good_rebuild_target(&foreground, "not-canonical")
                .await,
            Err(KnownGoodRebuildError::InvalidInstanceIdentity)
        ));
        drop((target, foreground));
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
        let (state, root) = state_fixture("verification-lease").await;
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
    async fn version_drift_and_config_only_root_mutation_fail_closed() {
        let (state, root) = state_fixture("version-root-drift").await;
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
                .postcheck_known_good_rebuild_target(
                    &foreground,
                    &version_target,
                    &test_contract(),
                )
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
                .postcheck_known_good_rebuild_target(&foreground, &root_target, &test_contract(),)
                .await,
            Err(KnownGoodRebuildError::LiveAuthorityMissing)
        );
        drop((root_target, version_target, foreground));
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn deletion_and_same_id_recreation_fail_registration_postcheck() {
        let (state, root) = state_fixture("delete-recreate").await;
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
                .postcheck_known_good_rebuild_target(
                    &foreground,
                    &deleted_target,
                    &test_contract(),
                )
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
                .postcheck_known_good_rebuild_target(
                    &foreground,
                    &recreated_target,
                    &test_contract(),
                )
                .await,
            Err(KnownGoodRebuildError::TargetChanged)
        );
        drop((recreated_target, deleted_target, foreground));
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn persisted_and_installed_evidence_never_suppresses_fresh_source_work() {
        let (state, root) = state_fixture("evidence-non-authority").await;
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
        let snapshot_dir = root.join("state/known-good");
        std::fs::create_dir_all(&snapshot_dir).expect("snapshot directory");
        std::fs::write(
            snapshot_dir.join(format!("{}.json", instance.id)),
            format!(
                "{{\"schema\":\"axial.state.known_good_inventory.v5\",\"instance_id\":\"{}\",\"version_id\":\"1.21.5\",\"activation_contract_id\":\"{}\",\"entries\":[{{\"root\":{{\"kind\":\"versions\"}},\"path\":\"1.21.5/1.21.5.json\",\"kind\":\"version_metadata\",\"integrity\":{{\"kind\":\"sha1\",\"digest\":\"0000000000000000000000000000000000000000\",\"size\":1}}}}]}}",
                instance.id,
                test_contract()
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
                .postcheck_known_good_rebuild_target(&foreground, &target, &test_contract(),)
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
        drop(target);
        drop(producer);
        drop(foreground);
        close_fixture(state, root).await;
    }
}
