use crate::{
    AUTHORITY_DRAINING, AUTHORITY_LIVE, CapabilityAuthority, CapabilityOperation, Directory,
    EntryKind, FileCapability, LeafName, LeafNameEquivalenceKey, MAX_DIRECTORY_LIST_ENTRIES,
    MAX_OUTSTANDING_EFFECTS, leaf_name_equivalence_keys, leaf_names_equivalent, platform,
    stale_capability,
};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::ops::ControlFlow;
use std::sync::Arc;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum TransientEffectPhase {
    Reserved,
    Live,
    Abandoned,
}

#[derive(Clone, Copy, Eq, PartialEq)]
pub(super) enum TransientEffectDisposition {
    Reserved,
    Staged,
    NoEffect,
    Published,
    Indeterminate,
}

pub(super) struct TransientEffectRecord {
    pub(super) directory: Directory,
    pub(super) destination: LeafName,
    pub(super) identity: Option<platform::Identity>,
    pub(super) retained: Option<platform::TransientFile>,
    pub(super) phase: TransientEffectPhase,
    pub(super) disposition: TransientEffectDisposition,
}

struct TransientEffectToken {
    id: u64,
    authority: Arc<CapabilityAuthority>,
    armed: bool,
}

struct DestinationBatchPlan {
    names: Vec<LeafName>,
    targets: HashMap<LeafNameEquivalenceKey, usize>,
}

impl DestinationBatchPlan {
    fn new(names: Vec<LeafName>) -> io::Result<Self> {
        if names.is_empty() || names.len() > MAX_OUTSTANDING_EFFECTS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "transient destination batch size is outside the supported range",
            ));
        }
        let mut targets = HashMap::with_capacity(names.len().saturating_mul(2));
        for (index, name) in names.iter().enumerate() {
            for key in leaf_name_equivalence_keys(name.as_os_str()) {
                if targets.insert(key, index).is_some() {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "transient destination batch contains portable aliases",
                    ));
                }
            }
        }
        Ok(Self { names, targets })
    }
}

impl TransientEffectToken {
    fn reserve_batch(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        directory: &Directory,
        plan: &DestinationBatchPlan,
    ) -> io::Result<Vec<Self>> {
        if !Arc::ptr_eq(authority, &operation.authority) {
            return Err(stale_capability());
        }
        let mut records = Vec::new();
        records
            .try_reserve_exact(plan.names.len())
            .map_err(|_| io::Error::other("transient effect record capacity is exhausted"))?;
        for name in &plan.names {
            records.push(TransientEffectRecord {
                directory: directory.clone(),
                destination: name.clone(),
                identity: None,
                retained: None,
                phase: TransientEffectPhase::Reserved,
                disposition: TransientEffectDisposition::Reserved,
            });
        }
        let mut tokens = Vec::new();
        tokens
            .try_reserve_exact(plan.names.len())
            .map_err(|_| io::Error::other("transient effect token capacity is exhausted"))?;
        let mut state = authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        for name in &plan.names {
            if transient_destination_is_reserved(&state, directory, name) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "transient destination is reserved by another filesystem effect",
                ));
            }
        }
        let first_id = state.next_transient_id;
        let count = u64::try_from(plan.names.len())
            .map_err(|_| io::Error::other("transient destination batch size overflowed"))?;
        let next_id = first_id
            .checked_add(count)
            .ok_or_else(|| io::Error::other("transient effect id overflowed"))?;
        for offset in 0..count {
            let id = first_id
                .checked_add(offset)
                .expect("prechecked transient effect id range");
            if state.transients.contains_key(&id) {
                return Err(io::Error::other(
                    "transient effect id is already registered",
                ));
            }
        }
        state
            .transients
            .try_reserve(plan.names.len())
            .map_err(|_| io::Error::other("transient effect registry capacity is exhausted"))?;
        state.reserve_effects(plan.names.len())?;
        state.next_transient_id = next_id;
        for (offset, record) in records.into_iter().enumerate() {
            let offset = u64::try_from(offset)
                .expect("bounded transient destination offset fits in u64");
            let id = first_id
                .checked_add(offset)
                .expect("prechecked transient effect id range");
            let previous = state.transients.insert(id, record);
            debug_assert!(previous.is_none(), "prechecked transient effect id is vacant");
            tokens.push(Self {
                id,
                authority: Arc::clone(authority),
                armed: true,
            });
        }
        Ok(tokens)
    }

    fn settle_no_effect_batch(
        tokens: &mut [Self],
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        let Some(first) = tokens.first() else {
            return Ok(());
        };
        for (index, token) in tokens.iter().enumerate() {
            if !token.armed
                || !Arc::ptr_eq(&token.authority, &first.authority)
                || !Arc::ptr_eq(&token.authority, &operation.authority)
                || tokens[..index]
                    .iter()
                    .any(|previous| previous.id == token.id)
            {
                return Err(stale_capability());
            }
        }
        let mut state = first.authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if state.active == 0 {
            return Err(stale_capability());
        }
        for token in tokens.iter() {
            let record = state.transients.get(&token.id).ok_or_else(stale_capability)?;
            if record.phase != TransientEffectPhase::Reserved
                || record.disposition != TransientEffectDisposition::Reserved
                || record.identity.is_some()
                || record.retained.is_some()
            {
                return Err(stale_capability());
            }
        }
        let outstanding_effects = state
            .outstanding_effects
            .checked_sub(tokens.len())
            .ok_or_else(stale_capability)?;
        for token in tokens.iter() {
            let removed = state.transients.remove(&token.id);
            debug_assert!(removed.is_some(), "prechecked transient effect is registered");
        }
        state.outstanding_effects = outstanding_effects;
        drop(state);
        for token in tokens {
            token.armed = false;
        }
        Ok(())
    }

    fn mark_live(&self, identity: platform::Identity) -> io::Result<()> {
        let mut state = self.authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        let record = state.transients.get_mut(&self.id).ok_or_else(stale_capability)?;
        if record.phase != TransientEffectPhase::Reserved {
            return Err(stale_capability());
        }
        record.phase = TransientEffectPhase::Live;
        record.identity = Some(identity);
        record.disposition = TransientEffectDisposition::Staged;
        Ok(())
    }

    fn mark_disposition(&self, disposition: TransientEffectDisposition) -> io::Result<()> {
        let mut state = self.authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        let record = state.transients.get_mut(&self.id).ok_or_else(stale_capability)?;
        if !matches!(record.phase, TransientEffectPhase::Reserved | TransientEffectPhase::Live) {
            return Err(stale_capability());
        }
        record.disposition = disposition;
        Ok(())
    }

    fn reset_reserved(&self) -> io::Result<()> {
        let mut state = self.authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        let record = state.transients.get_mut(&self.id).ok_or_else(stale_capability)?;
        if record.retained.is_some()
            || !matches!(record.phase, TransientEffectPhase::Reserved | TransientEffectPhase::Live)
            || !matches!(
                record.disposition,
                TransientEffectDisposition::Reserved
                    | TransientEffectDisposition::Staged
                    | TransientEffectDisposition::NoEffect
            )
        {
            return Err(stale_capability());
        }
        record.identity = None;
        record.phase = TransientEffectPhase::Reserved;
        record.disposition = TransientEffectDisposition::Reserved;
        Ok(())
    }

    fn mark_disposition_on_drop(&self, disposition: TransientEffectDisposition) {
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.transients.get_mut(&self.id) {
            record.disposition = disposition;
        }
    }

    fn abandon_with_retained(
        &mut self,
        retained: platform::TransientFile,
        disposition: TransientEffectDisposition,
    ) {
        assert!(
            self.armed,
            "retained transient authority requires an armed effect token"
        );
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let record = state
            .transients
            .get_mut(&self.id)
            .expect("armed transient effect retains its registry record");
        assert!(
            record.retained.is_none(),
            "transient effect registry retained duplicate native authority"
        );
        record.retained = Some(retained);
        record.disposition = disposition;
        record.phase = TransientEffectPhase::Abandoned;
        self.armed = false;
    }

    fn settle_with(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        if !Arc::ptr_eq(&self.authority, &operation.authority) {
            return Err(stale_capability());
        }
        self.authority.settle_transient_effect(self.id, operation)?;
        self.armed = false;
        Ok(())
    }

    fn abandon(&mut self) {
        if !self.armed {
            return;
        }
        self.authority.abandon_transient_effect(self.id);
        self.armed = false;
    }
}

impl Drop for TransientEffectToken {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn transient_destination_is_reserved(
    state: &crate::OperationState,
    candidate_directory: &Directory,
    candidate_name: &LeafName,
) -> bool {
    if state.file_parks_checked_out != 0 || state.directory_parks_checked_out != 0 {
        return true;
    }
    let conflicts_with_candidate = |directory: &Directory, name: &LeafName| {
        directory.inner.identity == candidate_directory.inner.identity
            && leaf_names_equivalent(name.as_os_str(), candidate_name.as_os_str())
    };
    state.moves.values().any(|movement| {
        crate::move_conflicts_with_transient(movement, candidate_directory, candidate_name)
    }) || state
        .transients
        .values()
        .any(|record| conflicts_with_candidate(&record.directory, &record.destination))
        || state.directory_creations.values().any(|record| {
            conflicts_with_candidate(&record.parent, &record.name)
        })
        || state.stage_creations.values().any(|record| {
            conflicts_with_candidate(&record.parent, &record.name)
        })
        || state.file_parks.values().any(|record| {
            conflicts_with_candidate(&record.parent, &record.original_name)
                || conflicts_with_candidate(&record.parent, &record.name)
        })
        || state.directory_parks.values().any(|record| {
            crate::directory_has_physical_ancestor(candidate_directory, record.identity)
                || conflicts_with_candidate(&record.parent, &record.original_name)
                || conflicts_with_candidate(&record.parent, &record.name)
        })
        || state.stages.values().any(|record| {
            conflicts_with_candidate(&record.parent, &record.name)
                || record.destination.as_ref().is_some_and(|target| {
                    conflicts_with_candidate(&target.parent, &target.name)
                })
        })
}

pub(super) fn transient_leaf_is_reserved(
    state: &crate::OperationState,
    directory: &Directory,
    name: &LeafName,
) -> bool {
    state.transients.values().any(|record| {
        record.directory.inner.identity == directory.inner.identity
            && leaf_names_equivalent(record.destination.as_os_str(), name.as_os_str())
    })
}

pub(super) fn transient_directory_identity_is_reserved(
    state: &crate::OperationState,
    identity: platform::Identity,
) -> bool {
    state.transients.values().any(|record| {
        crate::directory_has_physical_ancestor(&record.directory, identity)
    })
}

impl CapabilityAuthority {
    fn settle_transient_effect(
        &self,
        id: u64,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if !std::ptr::eq(operation.authority.as_ref(), self) {
            return Err(stale_capability());
        }
        let mut state = self.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        let terminal = state.transients.get(&id).is_some_and(|record| {
            record.retained.is_none()
                && matches!(
                    record.disposition,
                    TransientEffectDisposition::NoEffect | TransientEffectDisposition::Published
                )
        });
        if state.active == 0 || !terminal || state.transients.remove(&id).is_none() {
            return Err(stale_capability());
        }
        state.release_effect(operation);
        Ok(())
    }

    fn abandon_transient_effect(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.transients.get_mut(&id) {
            if record.phase == TransientEffectPhase::Reserved
                && record.disposition == TransientEffectDisposition::Reserved
            {
                record.disposition = TransientEffectDisposition::NoEffect;
            }
            record.phase = TransientEffectPhase::Abandoned;
        }
    }

    pub(super) fn cleanup_abandoned_transient(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (record, operation) = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let record = state.transients.remove(&id).ok_or_else(stale_capability)?;
            if record.phase != TransientEffectPhase::Abandoned {
                state.transients.insert(id, record);
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "transient effect authority is still live",
                ));
            }
            let Some(active) = state.active.checked_add(1) else {
                state.transients.insert(id, record);
                return Err(io::Error::other(
                    "filesystem capability operation count overflowed",
                ));
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                },
            )
        };
        let result = match (record.disposition, record.identity, record.retained.as_ref()) {
            (TransientEffectDisposition::NoEffect, _, None) => Ok(()),
            (
                TransientEffectDisposition::Published
                | TransientEffectDisposition::Indeterminate,
                Some(identity),
                Some(retained),
            ) => validate_terminal_publication(&record, retained, identity, &operation),
            _ => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "abandoned transient topology remains indeterminate",
            )),
        };
        if let Err(error) = result {
            self.operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .transients
                .insert(id, record);
            return Err(error);
        }
        self.operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .release_effect(&operation);
        Ok(())
    }
}

fn validate_terminal_publication(
    record: &TransientEffectRecord,
    retained: &platform::TransientFile,
    identity: platform::Identity,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let destination = TransientDestination {
        directory: record.directory.clone(),
        name: record.destination.clone(),
        token: None,
    };
    let validate = || {
        record.directory.validate(operation)?;
        if platform::transient_file_evidence(retained)? != (identity, 1)
            || platform::file_binding_state(
                &record.directory.inner.handle,
                record.destination.as_os_str(),
                identity,
            )? != platform::BindingState::Exact
            || !validate_portable_destination_with_operation(
                &destination,
                false,
                operation,
            )?
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "abandoned transient publication is not uniquely bound to its exact destination",
            ));
        }
        Ok(())
    };
    validate()?;
    platform::sync_directory(&record.directory.inner.handle)?;
    validate()
}

/// A destination admitted before external work begins.
///
/// Admission rejects occupied names and portable aliases. Publication is
/// create-only and performs fresh namespace checks around the durable effect.
#[must_use = "admitted transient destinations retain filesystem effect authority"]
pub struct TransientDestination {
    directory: Directory,
    name: LeafName,
    token: Option<TransientEffectToken>,
}

impl std::fmt::Debug for TransientDestination {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDestination")
            .finish_non_exhaustive()
    }
}

impl TransientDestination {
    pub fn name(&self) -> &LeafName {
        &self.name
    }

    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    pub fn create_stage(mut self) -> TransientStageCreateOutcome {
        let authority = match self.directory.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return TransientStageCreateOutcome::NoEffect {
                    error,
                    destination: self,
                };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return TransientStageCreateOutcome::NoEffect {
                    error,
                    destination: self,
                };
            }
        };
        let token = self
            .token
            .take()
            .expect("admitted transient destination retains its effect token");

        if let Err(error) = self.directory.validate(&operation) {
            self.token = Some(token);
            return TransientStageCreateOutcome::NoEffect {
                error,
                destination: self,
            };
        }
        match platform::create_transient_file(&self.directory.inner.handle) {
            Ok((file, identity)) => {
                let stage = TransientStage {
                    destination: self,
                    file: Some(file),
                    identity,
                    position: 0,
                    token: Some(token),
                };
                if let Err(error) = stage
                    .token
                    .as_ref()
                    .expect("created transient stage retains its effect token")
                    .mark_live(identity)
                {
                    return TransientStageCreateOutcome::Pending(
                        TransientCreationObligation {
                            error,
                            state: Some(TransientCreationState::Stage(stage)),
                        },
                    );
                }
                TransientStageCreateOutcome::Created(stage)
            }
            Err(platform::CreateTransientFileError::NoEffect(error)) => {
                self.token = Some(token);
                TransientStageCreateOutcome::NoEffect {
                    error,
                    destination: self,
                }
            }
        }
    }

    pub fn cancel(mut self) -> TransientDestinationCancelOutcome {
        let authority = match self.directory.authority() {
            Ok(authority) => authority,
            Err(error) => return pending_destination_cancel(error, self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return pending_destination_cancel(error, self),
        };
        let token = self
            .token
            .as_mut()
            .expect("admitted transient destination retains its effect token");
        match token
            .mark_disposition(TransientEffectDisposition::NoEffect)
            .and_then(|()| token.settle_with(&operation))
        {
            Ok(()) => TransientDestinationCancelOutcome::Cancelled,
            Err(error) => pending_destination_cancel(error, self),
        }
    }
}

impl Drop for TransientDestination {
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            token.mark_disposition_on_drop(TransientEffectDisposition::NoEffect);
        }
    }
}

/// An atomically admitted set of portable destination names in one directory.
#[must_use = "admitted transient destinations retain filesystem effect authority"]
pub struct TransientDestinationBatch {
    destinations: Vec<TransientDestination>,
}

impl std::fmt::Debug for TransientDestinationBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDestinationBatch")
            .field("len", &self.destinations.len())
            .finish_non_exhaustive()
    }
}

impl TransientDestinationBatch {
    pub fn len(&self) -> usize {
        self.destinations.len()
    }

    pub fn is_empty(&self) -> bool {
        self.destinations.is_empty()
    }

    pub fn into_destinations(self) -> Vec<TransientDestination> {
        self.destinations
    }
}

#[must_use = "transient destination cancellation may retain unsettled authority"]
pub enum TransientDestinationCancelOutcome {
    Cancelled,
    Pending(TransientDestinationCancelObligation),
}

impl std::fmt::Debug for TransientDestinationCancelOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDestinationCancelOutcome")
            .finish_non_exhaustive()
    }
}

#[must_use = "pending transient destination cancellation must be reconciled"]
pub struct TransientDestinationCancelObligation {
    error: io::Error,
    destination: Option<TransientDestination>,
}

impl std::fmt::Debug for TransientDestinationCancelObligation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDestinationCancelObligation")
            .finish_non_exhaustive()
    }
}

fn pending_destination_cancel(
    error: io::Error,
    destination: TransientDestination,
) -> TransientDestinationCancelOutcome {
    TransientDestinationCancelOutcome::Pending(TransientDestinationCancelObligation {
        error,
        destination: Some(destination),
    })
}

impl TransientDestinationCancelObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> TransientDestinationCancelOutcome {
        self.destination
            .take()
            .expect("destination cancellation obligation retains its authority")
            .cancel()
    }
}

impl Directory {
    pub fn admit_transient_destinations(
        &self,
        names: Vec<LeafName>,
    ) -> io::Result<TransientDestinationBatch> {
        let plan = DestinationBatchPlan::new(names)?;
        let mut destinations = Vec::new();
        destinations
            .try_reserve_exact(plan.names.len())
            .map_err(|_| io::Error::other("transient destination capacity is exhausted"))?;
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let mut tokens =
            TransientEffectToken::reserve_batch(&authority, &operation, self, &plan)?;
        if let Err(error) = validate_destination_batch_with_operation(
            self,
            &plan,
            true,
            &operation,
        ) {
            let cleanup = TransientEffectToken::settle_no_effect_batch(
                &mut tokens,
                &operation,
            );
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(io::Error::other(format!(
                    "transient destination admission failed: {error}; reservation cleanup remains pending: {cleanup}"
                ))),
            };
        }
        for (name, token) in plan.names.into_iter().zip(tokens) {
            destinations.push(TransientDestination {
                directory: self.clone(),
                name,
                token: Some(token),
            });
        }
        Ok(TransientDestinationBatch { destinations })
    }

    pub fn admit_transient_destination(
        &self,
        name: LeafName,
    ) -> io::Result<TransientDestination> {
        let mut destinations = self
            .admit_transient_destinations(vec![name])?
            .into_destinations();
        Ok(destinations
            .pop()
            .expect("singleton transient destination batch is nonempty"))
    }
}

fn enter_transient_operation(
    destination: &TransientDestination,
) -> io::Result<CapabilityOperation> {
    let authority = destination.directory.authority()?;
    let operation = authority.enter()?;
    destination.directory.validate(&operation)?;
    Ok(operation)
}

#[must_use = "transient stage creation outcomes must be handled"]
pub enum TransientStageCreateOutcome {
    Created(TransientStage),
    NoEffect {
        error: io::Error,
        destination: TransientDestination,
    },
    Pending(TransientCreationObligation),
}

impl std::fmt::Debug for TransientStageCreateOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientStageCreateOutcome")
            .finish_non_exhaustive()
    }
}

enum TransientCreationState {
    Stage(TransientStage),
}

#[must_use = "pending transient creation authority must be reconciled"]
pub struct TransientCreationObligation {
    error: io::Error,
    state: Option<TransientCreationState>,
}

impl std::fmt::Debug for TransientCreationObligation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientCreationObligation")
            .finish_non_exhaustive()
    }
}

impl TransientCreationObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> TransientStageCreateOutcome {
        match self
            .state
            .take()
            .expect("transient creation obligation retains its state")
        {
            TransientCreationState::Stage(stage) => {
                let result = stage
                    .token
                    .as_ref()
                    .expect("created transient stage retains its effect token")
                    .mark_live(stage.identity);
                match result {
                    Ok(()) => TransientStageCreateOutcome::Created(stage),
                    Err(error) => TransientStageCreateOutcome::Pending(Self {
                        error,
                        state: Some(TransientCreationState::Stage(stage)),
                    }),
                }
            }
        }
    }
}

#[must_use = "a transient stage must be sealed or explicitly discarded"]
pub struct TransientStage {
    destination: TransientDestination,
    file: Option<platform::TransientFile>,
    identity: platform::Identity,
    position: u64,
    token: Option<TransientEffectToken>,
}

impl std::fmt::Debug for TransientStage {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientStage")
            .field("bytes", &self.position)
            .finish_non_exhaustive()
    }
}

impl TransientStage {
    pub fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        let file = self.file.as_ref().ok_or_else(stale_capability)?;
        while !bytes.is_empty() {
            let written = platform::write_transient_at(file, bytes, self.position)?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "transient stage stopped accepting bytes",
                ));
            }
            self.position = self
                .position
                .checked_add(written as u64)
                .ok_or_else(|| io::Error::other("transient stage size overflowed"))?;
            bytes = &bytes[written..];
        }
        Ok(())
    }

    pub fn size(&self) -> u64 {
        self.position
    }

    pub fn seal(mut self) -> Result<TransientStageSealed, TransientStageSealFailure> {
        let file = self.file.as_mut().expect("live transient stage retains its file");
        if let Err(error) = platform::seal_transient_file(
            file,
            self.identity,
            self.position,
        ) {
            return Err(TransientStageSealFailure {
                error,
                stage: Some(self),
            });
        }
        Ok(TransientStageSealed {
            stage: self,
            read_position: 0,
        })
    }

    pub fn discard(mut self) -> TransientDiscardOutcome {
        let _operation = match enter_transient_operation(&self.destination) {
            Ok(operation) => operation,
            Err(error) => {
                return TransientDiscardOutcome::Pending(TransientDiscardObligation {
                    error,
                    state: Some(TransientDiscardState::Stage(self)),
                });
            }
        };
        let file = self.file.take().expect("live transient stage retains its file");
        match platform::discard_transient_file(file, self.identity) {
            Ok(()) => {
                let token = self
                    .token
                    .take()
                    .expect("live transient stage retains its effect token");
                let replacement = TransientDestination {
                    directory: self.destination.directory.clone(),
                    name: self.destination.name.clone(),
                    token: None,
                };
                let destination = std::mem::replace(&mut self.destination, replacement);
                restore_discarded_destination(destination, token)
            }
            Err(platform::DiscardTransientFileError::Retained { error, file }) => {
                self.file = Some(file);
                TransientDiscardOutcome::Pending(TransientDiscardObligation {
                    error,
                    state: Some(TransientDiscardState::Stage(self)),
                })
            }
        }
    }
}

impl Drop for TransientStage {
    fn drop(&mut self) {
        let Some(file) = self.file.take() else {
            return;
        };
        let topology = platform::transient_publication_state(
            &file,
            &self.destination.directory.inner.handle,
            self.destination.name.as_os_str(),
            self.identity,
        );
        match platform::discard_transient_file(file, self.identity) {
            Ok(()) => {
                if let Some(token) = self.token.as_ref() {
                    token.mark_disposition_on_drop(TransientEffectDisposition::NoEffect);
                }
            }
            Err(platform::DiscardTransientFileError::Retained { file, .. }) => {
                let disposition = match topology {
                    Ok(platform::TransientPublicationState::Published) => {
                        TransientEffectDisposition::Published
                    }
                    _ => TransientEffectDisposition::Indeterminate,
                };
                self.token
                    .as_mut()
                    .expect("retained transient stage retains its effect token")
                    .abandon_with_retained(file, disposition);
            }
        }
    }
}

#[must_use = "transient stage seal failures retain the stage"]
pub struct TransientStageSealFailure {
    error: io::Error,
    stage: Option<TransientStage>,
}

impl std::fmt::Debug for TransientStageSealFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientStageSealFailure")
            .finish_non_exhaustive()
    }
}

impl TransientStageSealFailure {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_stage(mut self) -> TransientStage {
        self.stage.take().expect("seal failure retains its stage")
    }
}

#[must_use = "a sealed transient stage must be published or explicitly discarded"]
pub struct TransientStageSealed {
    stage: TransientStage,
    read_position: u64,
}

impl std::fmt::Debug for TransientStageSealed {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientStageSealed")
            .finish_non_exhaustive()
    }
}

impl TransientStageSealed {
    pub fn size(&self) -> u64 {
        self.stage.position
    }

    pub fn discard(self) -> TransientDiscardOutcome {
        self.stage.discard()
    }

    pub fn publish_create_new(mut self) -> TransientPublicationOutcome {
        let operation = match enter_transient_operation(&self.stage.destination) {
            Ok(operation) => operation,
            Err(error) => {
                return TransientPublicationOutcome::NoEffect {
                    error,
                    stage: self,
                };
            }
        };
        if let Err(error) = validate_portable_destination_with_operation(
            &self.stage.destination,
            true,
            &operation,
        ) {
            return TransientPublicationOutcome::NoEffect {
                error,
                stage: self,
            };
        }
        let directory = &self.stage.destination.directory;
        let name = &self.stage.destination.name;
        let file = self
            .stage
            .file
            .as_mut()
            .expect("sealed transient stage retains its file");
        let link = platform::link_transient_file(
            file,
            &directory.inner.handle,
            name.as_os_str(),
        );
        let state = platform::transient_publication_state(
            file,
            &directory.inner.handle,
            name.as_os_str(),
            self.stage.identity,
        );
        match (link, state) {
            (Err(error), Ok(platform::TransientPublicationState::Unpublished)) => {
                TransientPublicationOutcome::NoEffect { error, stage: self }
            }
            (Err(_), Ok(platform::TransientPublicationState::Published)) => {
                settle_linked_stage(self, &operation)
            }
            (Err(error), _) => {
                TransientPublicationOutcome::Pending(TransientPublicationObligation {
                    error,
                    state: Some(TransientPublicationState::LinkUncertain(self)),
                })
            }
            (Ok(()), Ok(platform::TransientPublicationState::Published)) => {
                settle_linked_stage(self, &operation)
            }
            (Ok(()), _) => TransientPublicationOutcome::Pending(
                TransientPublicationObligation {
                    error: io::Error::other(
                        "transient publication reported success without an exact binding",
                    ),
                    state: Some(TransientPublicationState::Linked(self)),
                },
            ),
        }
    }
}

impl Read for TransientStageSealed {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let size = self.stage.position;
        if bytes.is_empty() || self.read_position == size {
            return Ok(0);
        }
        let remaining = size
            .checked_sub(self.read_position)
            .ok_or_else(|| io::Error::other("sealed transient reader position overflowed"))?;
        let requested = u64::try_from(bytes.len()).map_err(|_| {
            io::Error::other("sealed transient read length does not fit in a file offset")
        })?;
        let allowed = usize::try_from(remaining.min(requested)).map_err(|_| {
            io::Error::other("sealed transient read length does not fit this platform")
        })?;
        let file = self
            .stage
            .file
            .as_ref()
            .expect("sealed transient stage retains its file");
        let read = platform::read_transient_at(file, &mut bytes[..allowed], self.read_position)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "sealed transient file ended before its admitted size",
            ));
        }
        self.read_position = self
            .read_position
            .checked_add(u64::try_from(read).map_err(|_| {
                io::Error::other("sealed transient read result does not fit in a file offset")
            })?)
            .filter(|position| *position <= size)
            .ok_or_else(|| io::Error::other("sealed transient reader position overflowed"))?;
        Ok(read)
    }
}

impl Seek for TransientStageSealed {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let size = self.stage.position;
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(size) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(self.read_position) + i128::from(delta),
        };
        if !(0..=i128::from(size)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "sealed transient seek escaped its admitted range",
            ));
        }
        self.read_position = u64::try_from(next)
            .map_err(|_| io::Error::other("sealed transient reader position overflowed"))?;
        Ok(self.read_position)
    }
}

fn settle_linked_stage(
    sealed: TransientStageSealed,
    operation: &CapabilityOperation,
) -> TransientPublicationOutcome {
    let mut transition = TransientPublicationTransition::from_linked(sealed);
    if let Err(error) = transition.settle(operation) {
        return pending_published(error, transition);
    }
    TransientPublicationOutcome::Published(transition.into_file_capability())
}

#[cfg(all(test, target_os = "linux"))]
fn validate_linked_publication(
    sealed: &TransientStageSealed,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let destination = &sealed.stage.destination;
    destination.directory.validate(operation)?;
    let file = sealed
        .stage
        .file
        .as_ref()
        .expect("linked transient stage retains its file");
    validate_exact_destination(destination, file, sealed.stage.identity, operation)
}

fn validate_exact_destination(
    destination: &TransientDestination,
    retained: &platform::TransientFile,
    identity: platform::Identity,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    destination.directory.validate(operation)?;
    if platform::transient_file_evidence(retained)? != (identity, 1)
        || platform::file_binding_state(
            &destination.directory.inner.handle,
            destination.name.as_os_str(),
            identity,
        )? != platform::BindingState::Exact
        || !validate_portable_destination_with_operation(destination, false, operation)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transient publication did not retain its unique exact destination",
        ));
    }
    destination.directory.validate(operation)
}

fn validate_portable_destination_with_operation(
    destination: &TransientDestination,
    require_vacant: bool,
    operation: &CapabilityOperation,
) -> io::Result<bool> {
    let plan = DestinationBatchPlan::new(vec![destination.name.clone()])?;
    let exact = validate_destination_batch_with_operation(
        &destination.directory,
        &plan,
        require_vacant,
        operation,
    )?;
    Ok(exact[0])
}

fn validate_destination_batch_with_operation(
    directory: &Directory,
    plan: &DestinationBatchPlan,
    require_vacant: bool,
    operation: &CapabilityOperation,
) -> io::Result<Vec<bool>> {
    directory.validate(operation)?;
    let revision_before = platform::directory_revision(&directory.inner.handle)?;
    let mut exact = vec![false; plan.names.len()];
    let mut conflict = None;
    let visit = platform::visit_entries(
        &directory.inner.handle,
        MAX_DIRECTORY_LIST_ENTRIES,
        |observed_name, kind| {
            let target = leaf_name_equivalence_keys(observed_name)
                .into_iter()
                .find_map(|key| plan.targets.get(&key).copied());
            let Some(target) = target else {
                return Ok(ControlFlow::Continue(()));
            };
            let target_name = plan.names[target].as_os_str();
            let error = if observed_name != target_name || exact[target] {
                Some(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "transient destination has a portable alias",
                ))
            } else if kind != EntryKind::File {
                Some(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "transient destination is not a regular file",
                ))
            } else if require_vacant {
                Some(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "transient destination is already occupied",
                ))
            } else {
                exact[target] = true;
                None
            };
            if let Some(error) = error {
                conflict = Some(error);
                Ok(ControlFlow::Break(()))
            } else {
                Ok(ControlFlow::Continue(()))
            }
        },
    );
    directory.validate(operation)?;
    let revision_after = platform::directory_revision(&directory.inner.handle)?;
    let completion = visit?;
    if revision_after != revision_before {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "directory changed during transient destination inventory",
        ));
    }
    // Equal stamps never replace the complete inventory; they only fail a
    // proof when an observable namespace revision changed around the scan.
    match completion {
        platform::VisitCompletion::Complete => Ok(exact),
        platform::VisitCompletion::Stopped => Err(conflict.expect(
            "transient destination inventory stops only for a decisive conflict",
        )),
        platform::VisitCompletion::LimitExceeded => Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transient destination inventory exceeded its bound",
        )),
    }
}

#[must_use = "transient publication outcomes retain any unsettled effect"]
pub enum TransientPublicationOutcome {
    Published(FileCapability),
    NoEffect {
        error: io::Error,
        stage: TransientStageSealed,
    },
    Pending(TransientPublicationObligation),
}

impl std::fmt::Debug for TransientPublicationOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationOutcome")
            .finish_non_exhaustive()
    }
}

enum TransientPublicationState {
    LinkUncertain(TransientStageSealed),
    Linked(TransientStageSealed),
    Published(TransientPublicationTransition),
}

struct TransientPublicationTransition {
    stage: TransientStageSealed,
    destination: Option<TransientDestination>,
    identity: platform::Identity,
    retained: Option<platform::TransientFile>,
    token: Option<TransientEffectToken>,
}

impl TransientPublicationTransition {
    fn begin_linked(stage: TransientStageSealed) -> Self {
        let identity = stage.stage.identity;
        let mut transition = Self {
            stage,
            destination: None,
            identity,
            retained: None,
            token: None,
        };
        let destination = TransientDestination {
            directory: transition.stage.stage.destination.directory.clone(),
            name: transition.stage.stage.destination.name.clone(),
            token: None,
        };
        transition.destination = Some(destination);
        transition
    }

    fn from_linked(stage: TransientStageSealed) -> Self {
        let mut transition = Self::begin_linked(stage);
        transition.extract_retained();
        transition.extract_token();
        transition
    }

    fn extract_retained(&mut self) {
        assert!(
            self.retained.is_none(),
            "publication transition extracted duplicate native authority"
        );
        self.retained = self
            .stage
            .stage
            .file
            .take();
        assert!(
            self.retained.is_some(),
            "linked transient stage retains its native file"
        );
    }

    fn extract_token(&mut self) {
        assert!(
            self.token.is_none(),
            "publication transition extracted duplicate effect authority"
        );
        self.token = self
            .stage
            .stage
            .token
            .take();
        assert!(
            self.token.is_some(),
            "linked transient stage retains its effect token"
        );
    }

    fn destination(&self) -> &TransientDestination {
        self.destination
            .as_ref()
            .expect("publication transition retains its destination")
    }

    fn retained(&self) -> &platform::TransientFile {
        self.retained
            .as_ref()
            .expect("publication transition retains its native file")
    }

    fn token_mut(&mut self) -> &mut TransientEffectToken {
        self.token
            .as_mut()
            .expect("publication transition retains its effect token")
    }

    fn classify_published(&mut self) -> io::Result<()> {
        self.token_mut()
            .mark_disposition(TransientEffectDisposition::Published)
    }

    fn settle(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        validate_exact_destination(
            self.destination(),
            self.retained(),
            self.identity,
            operation,
        )?;
        platform::sync_directory(&self.destination().directory.inner.handle)?;
        validate_exact_destination(
            self.destination(),
            self.retained(),
            self.identity,
            operation,
        )?;
        self.classify_published()?;
        self.token_mut().settle_with(operation)
    }

    fn into_file_capability(mut self) -> FileCapability {
        assert!(
            !self
                .token
                .as_ref()
                .expect("publication transition retains its effect token")
                .armed,
            "published file capability requires a settled effect token"
        );
        let destination = self
            .destination
            .take()
            .expect("publication transition retains its destination");
        let authority = destination.directory.inner.authority.clone();
        let directory = destination.directory.clone();
        let name = destination.name.clone();
        drop(destination);
        let retained = self
            .retained
            .take()
            .expect("publication transition retains its native file");
        drop(self.token.take());
        FileCapability::new(
            platform::into_published_file(retained),
            self.identity,
            directory,
            name,
            authority,
        )
    }
}

impl Drop for TransientPublicationTransition {
    fn drop(&mut self) {
        let armed = self
            .token
            .as_ref()
            .or(self.stage.stage.token.as_ref())
            .is_some_and(|token| token.armed);
        if !armed {
            drop(self.retained.take());
            drop(self.stage.stage.file.take());
            return;
        }
        if self.stage.stage.file.is_none() {
            self.stage.stage.file = self.retained.take();
        }
        if self.stage.stage.token.is_none() {
            self.stage.stage.token = self.token.take();
        }
    }
}

#[must_use = "pending transient publication authority must be reconciled"]
pub struct TransientPublicationObligation {
    error: io::Error,
    state: Option<TransientPublicationState>,
}

impl std::fmt::Debug for TransientPublicationObligation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationObligation")
            .finish_non_exhaustive()
    }
}

fn pending_published(
    error: io::Error,
    transition: TransientPublicationTransition,
) -> TransientPublicationOutcome {
    TransientPublicationOutcome::Pending(TransientPublicationObligation {
        error,
        state: Some(TransientPublicationState::Published(transition)),
    })
}

impl TransientPublicationObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    fn take_error(&mut self) -> io::Error {
        std::mem::replace(
            &mut self.error,
            io::Error::other("transient publication obligation was consumed"),
        )
    }

    pub fn reconcile(mut self) -> TransientPublicationOutcome {
        match self
            .state
            .take()
            .expect("publication obligation retains its state")
        {
            TransientPublicationState::LinkUncertain(stage) => {
                let operation = match enter_transient_operation(&stage.stage.destination) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return TransientPublicationOutcome::Pending(
                            TransientPublicationObligation {
                                error,
                                state: Some(TransientPublicationState::LinkUncertain(stage)),
                            },
                        );
                    }
                };
                let destination = &stage.stage.destination;
                let file = stage
                    .stage
                    .file
                    .as_ref()
                    .expect("uncertain transient publication retains its file");
                match platform::transient_publication_state(
                    file,
                    &destination.directory.inner.handle,
                    destination.name.as_os_str(),
                    stage.stage.identity,
                ) {
                    Ok(platform::TransientPublicationState::Published) => {
                        settle_linked_stage(stage, &operation)
                    }
                    Ok(platform::TransientPublicationState::Unpublished) => {
                        TransientPublicationOutcome::NoEffect {
                            error: self.take_error(),
                            stage,
                        }
                    }
                    _ => TransientPublicationOutcome::Pending(TransientPublicationObligation {
                        error: self.take_error(),
                        state: Some(TransientPublicationState::LinkUncertain(stage)),
                    }),
                }
            }
            TransientPublicationState::Linked(stage) => {
                let operation = match enter_transient_operation(&stage.stage.destination) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return TransientPublicationOutcome::Pending(
                            TransientPublicationObligation {
                                error,
                                state: Some(TransientPublicationState::Linked(stage)),
                            },
                        );
                    }
                };
                settle_linked_stage(stage, &operation)
            }
            TransientPublicationState::Published(mut transition) => {
                let operation = match enter_transient_operation(transition.destination()) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return pending_published(error, transition);
                    }
                };
                match transition.settle(&operation) {
                    Ok(()) => TransientPublicationOutcome::Published(
                        transition.into_file_capability(),
                    ),
                    Err(error) => pending_published(error, transition),
                }
            }
        }
    }
}

#[must_use = "transient discard outcomes must retain failed cleanup authority"]
pub enum TransientDiscardOutcome {
    Discarded(TransientDestination),
    Pending(TransientDiscardObligation),
}

impl std::fmt::Debug for TransientDiscardOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDiscardOutcome")
            .finish_non_exhaustive()
    }
}

#[must_use = "pending transient discard authority must be reconciled"]
pub struct TransientDiscardObligation {
    error: io::Error,
    state: Option<TransientDiscardState>,
}

enum TransientDiscardState {
    Stage(TransientStage),
    ReservationRestore {
        destination: TransientDestination,
        token: TransientEffectToken,
    },
}

impl std::fmt::Debug for TransientDiscardObligation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientDiscardObligation")
            .finish_non_exhaustive()
    }
}

impl TransientDiscardObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> TransientDiscardOutcome {
        match self
            .state
            .take()
            .expect("discard obligation retains its state")
        {
            TransientDiscardState::Stage(stage) => stage.discard(),
            TransientDiscardState::ReservationRestore { destination, token } => {
                restore_discarded_destination(destination, token)
            }
        }
    }
}

fn restore_discarded_destination(
    mut destination: TransientDestination,
    token: TransientEffectToken,
) -> TransientDiscardOutcome {
    token.mark_disposition_on_drop(TransientEffectDisposition::NoEffect);
    match token.reset_reserved() {
        Ok(()) => {
            destination.token = Some(token);
            TransientDiscardOutcome::Discarded(destination)
        }
        Err(error) => TransientDiscardOutcome::Pending(TransientDiscardObligation {
            error,
            state: Some(TransientDiscardState::ReservationRestore { destination, token }),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MoveEffectRecord, MoveEffectToken, NamespaceLeaf, RootRevokeOutcome, RootSession,
        RootSessionAcquireOutcome, move_conflicts_with_transient,
    };
    use std::ffi::OsStr;
    use std::io::{Read as _, Seek as _, SeekFrom};
    use std::sync::{Arc, Barrier};
    use std::thread;

    fn acquire_test_root(path: &std::path::Path) -> RootSession {
        match RootSession::acquire(path) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            RootSessionAcquireOutcome::NoEffect(error) => {
                panic!("root acquisition had no effect: {error}")
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                panic!("root acquisition is unsettled: {}", obligation.error())
            }
        }
    }

    fn test_stage(root: &Directory, name: &str) -> Option<TransientStage> {
        let destination = root
            .admit_transient_destination(LeafName::new(name).expect("transient leaf"))
            .expect("transient destination admission");
        match destination.create_stage() {
            TransientStageCreateOutcome::Created(stage) => Some(stage),
            TransientStageCreateOutcome::NoEffect { error, .. }
                if matches!(
                    error.kind(),
                    io::ErrorKind::Unsupported | io::ErrorKind::PermissionDenied
                ) =>
            {
                None
            }
            TransientStageCreateOutcome::NoEffect { error, .. } => {
                panic!("transient creation had no effect: {error}")
            }
            TransientStageCreateOutcome::Pending(obligation) => {
                panic!("transient creation is unsettled: {}", obligation.error())
            }
        }
    }

    fn namespace_leaf(parent: &Directory, name: &str) -> NamespaceLeaf {
        NamespaceLeaf {
            parent: parent.clone(),
            name: LeafName::new(name).expect("namespace leaf"),
        }
    }

    fn reserve_test_transient(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        directory: &Directory,
        name: &str,
    ) -> io::Result<TransientEffectToken> {
        let plan = DestinationBatchPlan::new(vec![
            LeafName::new(name).expect("transient leaf"),
        ])?;
        let mut tokens =
            TransientEffectToken::reserve_batch(authority, operation, directory, &plan)?;
        Ok(tokens
            .pop()
            .expect("singleton transient reservation is nonempty"))
    }

    #[test]
    fn batch_aliases_are_rejected_before_effect_reservation() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let error = root
            .admit_transient_destinations(vec![
                LeafName::new("Artifact.bin").expect("first batch leaf"),
                LeafName::new("artifact.BIN").expect("alias batch leaf"),
            ])
            .expect_err("portable aliases must not be admitted together");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("filesystem operation state");
            assert!(state.transients.is_empty());
            assert_eq!(state.outstanding_effects, 0);
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn external_batch_collision_settles_every_reservation() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        std::fs::write(temporary.path().join("Occupied.bin"), b"occupied")
            .expect("external occupied file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let error = root
            .admit_transient_destinations(vec![
                LeafName::new("occupied.BIN").expect("occupied alias leaf"),
                LeafName::new("vacant.bin").expect("vacant batch leaf"),
            ])
            .expect_err("external portable alias must reject the batch");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("filesystem operation state");
            assert!(state.transients.is_empty());
            assert_eq!(state.outstanding_effects, 0);
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn held_destination_blocks_batch_until_explicit_cancellation() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let held = root
            .admit_transient_destination(
                LeafName::new("Held.bin").expect("held destination leaf"),
            )
            .expect("held destination admission");
        let error = root
            .admit_transient_destinations(vec![
                LeafName::new("held.BIN").expect("held destination alias"),
            ])
            .expect_err("held destination must block a competing batch");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        match held.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("held destination cancellation remained pending: {}", obligation.error())
            }
        }
        let mut retried = root
            .admit_transient_destinations(vec![
                LeafName::new("held.BIN").expect("retried destination leaf"),
            ])
            .expect("destination admission after cancellation")
            .into_destinations();
        let retried = retried
            .pop()
            .expect("retried singleton batch is nonempty");
        match retried.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("retried destination cancellation remained pending: {}", obligation.error())
            }
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn batch_admission_reserves_every_destination_atomically() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let batch = root
            .admit_transient_destinations(vec![
                LeafName::new("first.bin").expect("first batch leaf"),
                LeafName::new("second.bin").expect("second batch leaf"),
            ])
            .expect("transient destination batch admission");
        assert_eq!(batch.len(), 2);
        assert!(!batch.is_empty());
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("filesystem operation state");
            assert_eq!(state.transients.len(), 2);
            assert_eq!(state.outstanding_effects, 2);
            assert!(state.transients.values().all(|record| {
                record.phase == TransientEffectPhase::Reserved
                    && record.disposition == TransientEffectDisposition::Reserved
            }));
        }
        drop(batch);
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("filesystem operation state");
            assert!(state.transients.values().all(|record| {
                record.phase == TransientEffectPhase::Abandoned
                    && record.disposition == TransientEffectDisposition::NoEffect
            }));
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn explicit_destination_cancellation_releases_its_reservation() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let destination = root
            .admit_transient_destination(
                LeafName::new("cancelled.bin").expect("cancelled destination leaf"),
            )
            .expect("cancelled destination admission");
        match destination.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("destination cancellation remained pending: {}", obligation.error())
            }
        }
        let state = session
            .authority
            .operations
            .lock()
            .expect("filesystem operation state");
        assert!(state.transients.is_empty());
        assert_eq!(state.outstanding_effects, 0);
        drop(state);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discarded_stage_reuses_the_exact_destination_reservation() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let Some(first) = test_stage(&root, "retry.bin") else {
            drop(root);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        };
        let reservation_id = first
            .token
            .as_ref()
            .expect("first stage effect token")
            .id;
        let destination = match first.discard() {
            TransientDiscardOutcome::Discarded(destination) => destination,
            TransientDiscardOutcome::Pending(obligation) => {
                panic!("first stage discard remained pending: {}", obligation.error())
            }
        };
        assert_eq!(
            destination
                .token
                .as_ref()
                .expect("discarded stage returned its destination token")
                .id,
            reservation_id,
        );
        let error = root
            .admit_transient_destinations(vec![
                LeafName::new("RETRY.BIN").expect("retry destination alias"),
            ])
            .expect_err("discarded destination must retain its reservation");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let second = match destination.create_stage() {
            TransientStageCreateOutcome::Created(stage) => stage,
            TransientStageCreateOutcome::NoEffect { error, .. } => {
                panic!("second stage creation had no effect: {error}")
            }
            TransientStageCreateOutcome::Pending(obligation) => {
                panic!("second stage creation remained pending: {}", obligation.error())
            }
        };
        assert_eq!(
            second
                .token
                .as_ref()
                .expect("second stage effect token")
                .id,
            reservation_id,
        );
        let destination = match second.discard() {
            TransientDiscardOutcome::Discarded(destination) => destination,
            TransientDiscardOutcome::Pending(obligation) => {
                panic!("second stage discard remained pending: {}", obligation.error())
            }
        };
        match destination.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("retried destination cancellation remained pending: {}", obligation.error())
            }
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn reserved_token_unwind_is_root_cleanable_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let authority = session.authority.clone();
        let operation = authority.enter().expect("reservation operation");
        let token = reserve_test_transient(
            &authority,
            &operation,
            &root,
            "unwound-reservation.bin",
        )
        .expect("transient reservation");
        drop(token);
        drop(operation);
        {
            let state = authority
                .operations
                .lock()
                .expect("filesystem operation state");
            let record = state
                .transients
                .values()
                .next()
                .expect("abandoned reservation record");
            assert!(record.phase == TransientEffectPhase::Abandoned);
            assert!(record.disposition == TransientEffectDisposition::NoEffect);
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn move_conflicts_cover_portable_source_and_destination_aliases() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let movement = MoveEffectRecord {
            source: namespace_leaf(&root, "Source.bin"),
            destination: namespace_leaf(&root, "Destination.bin"),
            moved_directory: None,
        };

        assert!(move_conflicts_with_transient(
            &movement,
            &root,
            &LeafName::new("SOURCE.BIN").expect("source alias"),
        ));
        assert!(move_conflicts_with_transient(
            &movement,
            &root,
            &LeafName::new("destination.BIN").expect("destination alias"),
        ));
        assert!(!move_conflicts_with_transient(
            &movement,
            &root,
            &LeafName::new("sibling.bin").expect("sibling leaf"),
        ));

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn directory_moves_conflict_with_descendants_but_not_sibling_trees() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        std::fs::create_dir_all(temporary.path().join("moved/nested"))
            .expect("moved descendant");
        std::fs::create_dir(temporary.path().join("sibling")).expect("sibling directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let moved = root
            .open_directory(&LeafName::new("moved").expect("moved leaf"))
            .expect("moved directory");
        let nested = moved
            .open_directory(&LeafName::new("nested").expect("nested leaf"))
            .expect("nested directory");
        let sibling = root
            .open_directory(&LeafName::new("sibling").expect("sibling leaf"))
            .expect("sibling directory");
        let movement = MoveEffectRecord {
            source: namespace_leaf(&root, "moved"),
            destination: namespace_leaf(&root, "renamed"),
            moved_directory: Some(moved.inner.identity.physical),
        };

        assert!(move_conflicts_with_transient(
            &movement,
            &nested,
            &LeafName::new("payload.bin").expect("nested payload"),
        ));
        assert!(!move_conflicts_with_transient(
            &movement,
            &sibling,
            &LeafName::new("payload.bin").expect("sibling payload"),
        ));

        drop((nested, moved, sibling, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn move_and_transient_reservations_reject_conflicts_in_either_order() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let authority = session.authority.clone();

        {
            let operation = authority.enter().expect("move-first operation");
            let mut movement = MoveEffectToken::reserve(
                &authority,
                &operation,
                namespace_leaf(&root, "source.bin"),
                namespace_leaf(&root, "Destination.bin"),
                None,
            )
            .expect("move-first reservation");
            let conflict = reserve_test_transient(
                &authority,
                &operation,
                &root,
                "destination.BIN",
            );
            match conflict {
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                Ok(mut unexpected) => {
                    unexpected
                        .mark_disposition(TransientEffectDisposition::NoEffect)
                        .expect("unexpected transient disposition");
                    unexpected
                        .settle_with(&operation)
                        .expect("unexpected transient settlement");
                    movement
                        .settle(&operation)
                        .expect("move-first cleanup settlement");
                    panic!("move-first conflict was admitted");
                }
            }
            movement.settle(&operation).expect("move-first settlement");
        }

        {
            let operation = authority.enter().expect("transient-first operation");
            let mut transient = reserve_test_transient(
                &authority,
                &operation,
                &root,
                "Source.bin",
            )
            .expect("transient-first reservation");
            let conflict = MoveEffectToken::reserve(
                &authority,
                &operation,
                namespace_leaf(&root, "source.BIN"),
                namespace_leaf(&root, "other.bin"),
                None,
            );
            match conflict {
                Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
                Ok(mut unexpected) => {
                    unexpected
                        .settle(&operation)
                        .expect("unexpected move settlement");
                    transient
                        .mark_disposition(TransientEffectDisposition::NoEffect)
                        .expect("transient-first cleanup disposition");
                    transient
                        .settle_with(&operation)
                        .expect("transient-first cleanup settlement");
                    panic!("transient-first conflict was admitted");
                }
            }
            transient
                .mark_disposition(TransientEffectDisposition::NoEffect)
                .expect("transient disposition");
            transient
                .settle_with(&operation)
                .expect("transient-first settlement");
        }

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn unrelated_sibling_tree_reservations_proceed_together() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        std::fs::create_dir(temporary.path().join("move-tree")).expect("move tree");
        std::fs::create_dir(temporary.path().join("transient-tree"))
            .expect("transient tree");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let move_tree = root
            .open_directory(&LeafName::new("move-tree").expect("move tree leaf"))
            .expect("move tree directory");
        let transient_tree = root
            .open_directory(&LeafName::new("transient-tree").expect("transient tree leaf"))
            .expect("transient tree directory");
        let authority = session.authority.clone();
        let operation = authority.enter().expect("sibling reservation operation");
        let mut movement = MoveEffectToken::reserve(
            &authority,
            &operation,
            namespace_leaf(&move_tree, "source.bin"),
            namespace_leaf(&move_tree, "destination.bin"),
            None,
        )
        .expect("sibling move reservation");
        let mut transient = reserve_test_transient(
            &authority,
            &operation,
            &transient_tree,
            "destination.bin",
        )
        .expect("unrelated transient reservation");

        transient
            .mark_disposition(TransientEffectDisposition::NoEffect)
            .expect("transient disposition");
        transient
            .settle_with(&operation)
            .expect("transient settlement");
        movement.settle(&operation).expect("move settlement");
        drop(operation);
        drop((transient_tree, move_tree, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn simultaneous_move_and_transient_reservations_admit_exactly_one() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let authority = session.authority.clone();
        let start = Arc::new(Barrier::new(2));
        let finish = Arc::new(Barrier::new(2));

        let move_thread = {
            let authority = Arc::clone(&authority);
            let root = root.clone();
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            thread::spawn(move || {
                let operation = authority.enter().expect("move race operation");
                start.wait();
                let reservation = MoveEffectToken::reserve(
                    &authority,
                    &operation,
                    namespace_leaf(&root, "source.bin"),
                    namespace_leaf(&root, "Race.bin"),
                    None,
                );
                finish.wait();
                match reservation {
                    Ok(mut token) => {
                        token.settle(&operation).expect("move race settlement");
                        true
                    }
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                        false
                    }
                }
            })
        };
        let transient_thread = {
            let authority = Arc::clone(&authority);
            let root = root.clone();
            let start = Arc::clone(&start);
            let finish = Arc::clone(&finish);
            thread::spawn(move || {
                let operation = authority.enter().expect("transient race operation");
                start.wait();
                let reservation = reserve_test_transient(
                    &authority,
                    &operation,
                    &root,
                    "race.BIN",
                );
                finish.wait();
                match reservation {
                    Ok(mut token) => {
                        token
                            .mark_disposition(TransientEffectDisposition::NoEffect)
                            .expect("transient race disposition");
                        token
                            .settle_with(&operation)
                            .expect("transient race settlement");
                        true
                    }
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                        false
                    }
                }
            })
        };

        let move_admitted = move_thread.join().expect("move race thread");
        let transient_admitted = transient_thread.join().expect("transient race thread");
        assert_ne!(move_admitted, transient_admitted);
        {
            let state = authority.operations.lock().expect("settled race state");
            assert!(state.moves.is_empty());
            assert!(state.transients.is_empty());
            assert_eq!(state.outstanding_effects, 0);
        }

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    fn linked_test_stage(
        root: &Directory,
        name: &str,
    ) -> (TransientStageSealed, u64) {
        let mut sealed = test_stage(root, name)
            .expect("transient platform")
            .seal()
            .expect("sealed transient stage");
        platform::link_transient_file(
            sealed
                .stage
                .file
                .as_mut()
                .expect("sealed stage retains native file"),
            &root.inner.handle,
            OsStr::new(name),
        )
        .expect("linked transient stage");
        let id = sealed
            .stage
            .token
            .as_ref()
            .expect("linked stage retains effect token")
            .id;
        (sealed, id)
    }

    #[cfg(target_os = "linux")]
    fn assert_retained_transient(
        session: &RootSession,
        id: u64,
        disposition: TransientEffectDisposition,
    ) {
        let state = session
            .authority
            .operations
            .lock()
            .expect("filesystem operation state");
        let record = state.transients.get(&id).expect("retained transient record");
        assert!(record.phase == TransientEffectPhase::Abandoned);
        assert!(record.disposition == disposition);
        assert!(record.retained.is_some());
    }

    #[test]
    fn dropped_pending_carriers_remain_root_owned_until_terminal_cleanup() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");

        let Some(create_stage) = test_stage(&root, "create-pending.bin") else {
            drop(root);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        };
        let create_pending = TransientCreationObligation {
            error: io::Error::other("injected creation settlement"),
            state: Some(TransientCreationState::Stage(create_stage)),
        };
        drop(create_pending);

        let publication_stage = test_stage(&root, "publication-pending.bin")
            .expect("transient platform remained available");
        let publication_pending = TransientPublicationObligation {
            error: io::Error::other("injected publication settlement"),
            state: Some(TransientPublicationState::LinkUncertain(
                publication_stage.seal().expect("sealed publication stage"),
            )),
        };
        drop(publication_pending);

        let discard_stage = test_stage(&root, "discard-pending.bin")
            .expect("transient platform remained available");
        let discard_pending = TransientDiscardObligation {
            error: io::Error::other("injected discard settlement"),
            state: Some(TransientDiscardState::Stage(discard_stage)),
        };
        drop(discard_pending);

        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("filesystem operation state");
            assert_eq!(state.transients.len(), 3);
            assert_eq!(state.outstanding_effects, 3);
            assert!(
                state
                    .transients
                    .values()
                    .all(|record| record.phase == TransientEffectPhase::Abandoned)
            );
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn anonymous_stage_publishes_exact_single_link_content() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let Some(mut stage) = test_stage(&root, "published.bin") else {
            drop(root);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        };
        stage.write_all(b"managed payload").expect("stream stage write");
        let sealed = stage.seal().expect("stream stage seal");
        let published = match sealed.publish_create_new() {
            TransientPublicationOutcome::Published(file) => file,
            TransientPublicationOutcome::NoEffect { error, .. } => {
                panic!("publication had no effect: {error}")
            }
            TransientPublicationOutcome::Pending(obligation) => {
                panic!("publication remained pending: {}", obligation.error())
            }
        };
        assert_eq!(
            std::fs::read(temporary.path().join("published.bin"))
                .expect("published payload read"),
            b"managed payload",
        );
        assert_eq!(
            platform::exact_file_link_count(
                &root.inner.handle,
                OsStr::new("published.bin"),
                published.identity,
            )
            .expect("published link count"),
            Some(1),
        );
        drop(published);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_stage_reads_and_seeks_within_its_admitted_size_before_publication() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let Some(mut stage) = test_stage(&root, "readable.bin") else {
            drop(root);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            return;
        };
        stage.write_all(b"0123456789").expect("stream stage write");
        let mut sealed = stage.seal().expect("stream stage seal");

        let mut prefix = [0_u8; 4];
        sealed.read_exact(&mut prefix).expect("sealed prefix read");
        assert_eq!(&prefix, b"0123");
        assert_eq!(sealed.seek(SeekFrom::Current(2)).expect("forward seek"), 6);
        let mut suffix = Vec::new();
        sealed.read_to_end(&mut suffix).expect("sealed suffix read");
        assert_eq!(suffix, b"6789");
        assert!(sealed.seek(SeekFrom::Start(11)).is_err());
        assert!(sealed.seek(SeekFrom::End(-11)).is_err());
        assert_eq!(sealed.stream_position().expect("retained cursor"), 10);
        assert_eq!(sealed.seek(SeekFrom::End(-3)).expect("tail seek"), 7);
        let mut tail = [0_u8; 3];
        sealed.read_exact(&mut tail).expect("sealed tail read");
        assert_eq!(&tail, b"789");
        assert_eq!(sealed.read(&mut prefix).expect("bounded eof"), 0);

        let published = match sealed.publish_create_new() {
            TransientPublicationOutcome::Published(file) => file,
            TransientPublicationOutcome::NoEffect { error, .. } => {
                panic!("publication had no effect: {error}")
            }
            TransientPublicationOutcome::Pending(obligation) => {
                panic!("publication remained pending: {}", obligation.error())
            }
        };
        assert_eq!(
            std::fs::read(temporary.path().join("readable.bin"))
                .expect("published readable payload"),
            b"0123456789",
        );
        drop(published);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropped_published_obligation_transfers_exact_handle_to_root() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let (sealed, id) = linked_test_stage(&root, "root-retained.bin");
        let mut transition = TransientPublicationTransition::from_linked(sealed);
        transition
            .classify_published()
            .expect("published transition classification");
        let obligation = TransientPublicationObligation {
            error: io::Error::other("injected published settlement"),
            state: Some(TransientPublicationState::Published(transition)),
        };
        drop(obligation);

        assert_retained_transient(&session, id, TransientEffectDisposition::Published);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn publication_transition_unwind_after_carrier_extraction_retains_root_authority() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let (sealed, id) = linked_test_stage(&root, "extraction-unwind.bin");

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut transition = TransientPublicationTransition::begin_linked(sealed);
            transition.extract_retained();
            panic!("injected unwind after native carrier extraction");
        }));
        assert!(unwind.is_err());
        assert_retained_transient(
            &session,
            id,
            TransientEffectDisposition::Published,
        );
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn publication_transition_unwind_after_classification_retains_root_authority() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let (sealed, id) = linked_test_stage(&root, "classification-unwind.bin");

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut transition = TransientPublicationTransition::from_linked(sealed);
            transition
                .classify_published()
                .expect("published transition classification");
            panic!("injected unwind after publication classification");
        }));
        assert!(unwind.is_err());
        assert_retained_transient(&session, id, TransientEffectDisposition::Published);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn held_transient_rejects_replacement_then_relinks_and_settles() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let mut stage = test_stage(&root, "aba.bin").expect("transient platform");
        stage.write_all(b"held original").expect("original write");
        let mut sealed = stage.seal().expect("sealed original");
        platform::link_transient_file(
            sealed
                .stage
                .file
                .as_mut()
                .expect("sealed stage retains native file"),
            &root.inner.handle,
            OsStr::new("aba.bin"),
        )
        .expect("initial original link");

        std::fs::remove_file(temporary.path().join("aba.bin")).expect("unlink original name");
        std::fs::write(temporary.path().join("aba.bin"), b"replacement")
            .expect("install replacement");
        {
            let operation = enter_transient_operation(&sealed.stage.destination)
                .expect("transient validation operation");
            assert!(validate_linked_publication(&sealed, &operation).is_err());
        }
        assert_eq!(
            std::fs::read(temporary.path().join("aba.bin")).expect("replacement read"),
            b"replacement",
        );

        std::fs::remove_file(temporary.path().join("aba.bin")).expect("remove replacement");
        platform::link_transient_file(
            sealed
                .stage
                .file
                .as_mut()
                .expect("held original remains available"),
            &root.inner.handle,
            OsStr::new("aba.bin"),
        )
        .expect("relink held original");
        let published = {
            let operation = enter_transient_operation(&sealed.stage.destination)
                .expect("transient settlement operation");
            match settle_linked_stage(sealed, &operation) {
                TransientPublicationOutcome::Published(file) => file,
                TransientPublicationOutcome::NoEffect { error, .. } => {
                    panic!("relinked publication had no effect: {error}")
                }
                TransientPublicationOutcome::Pending(obligation) => {
                    panic!("relinked publication remained pending: {}", obligation.error())
                }
            }
        };
        assert_eq!(
            std::fs::read(temporary.path().join("aba.bin")).expect("original read"),
            b"held original",
        );
        drop(published);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepublication_collision_is_deferred_to_publish_and_preserves_the_stage() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let destination = root
            .admit_transient_destination(LeafName::new("collision.bin").expect("collision leaf"))
            .expect("collision destination admission");
        std::fs::write(temporary.path().join("COLLISION.BIN"), b"user payload")
            .expect("portable collision injection");
        let stage = match destination.create_stage() {
            TransientStageCreateOutcome::Created(stage) => stage,
            TransientStageCreateOutcome::NoEffect { error, .. } => {
                panic!("collision prevented namespace-independent staging: {error}")
            }
            TransientStageCreateOutcome::Pending(obligation) => {
                panic!("collision admission remained pending: {}", obligation.error())
            }
        };
        let sealed = stage.seal().expect("collision stage seal");
        let preserved = match sealed.publish_create_new() {
            TransientPublicationOutcome::NoEffect { stage, .. } => stage,
            TransientPublicationOutcome::Published(_) => {
                panic!("portable collision unexpectedly published")
            }
            TransientPublicationOutcome::Pending(obligation) => {
                panic!("collision publication remained pending: {}", obligation.error())
            }
        };
        let destination = match preserved.discard() {
            TransientDiscardOutcome::Discarded(destination) => destination,
            TransientDiscardOutcome::Pending(obligation) => {
                panic!("collision stage discard remained pending: {}", obligation.error())
            }
        };
        match destination.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("collision destination cancellation remained pending: {}", obligation.error())
            }
        }
        assert_eq!(
            std::fs::read(temporary.path().join("COLLISION.BIN"))
                .expect("collision payload read"),
            b"user payload",
        );
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

}
