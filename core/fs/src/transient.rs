use crate::{
    AUTHORITY_DRAINING, AUTHORITY_LIVE, CapabilityAuthority, CapabilityOperation, Directory,
    EntryKind, FileCapability, LeafName, LeafNameEquivalenceKey, MAX_DIRECTORY_LIST_ENTRIES,
    MAX_LEAF_UNITS, MAX_OUTSTANDING_EFFECTS, leaf_name_equivalence_keys, leaf_names_equivalent,
    platform, stale_capability,
};
#[cfg(target_os = "linux")]
use crate::{AUTHORITY_QUIESCING, terminal_effect_settlement_admits};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::ops::ControlFlow;
use std::sync::Arc;

// Unicode folding and canonical decomposition can expand one admitted input
// unit into several UTF-8 scalars.
const MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES: usize = 1 + MAX_LEAF_UNITS * 16;

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

struct TransientDestinationToken {
    token: Option<TransientEffectToken>,
}

impl TransientDestinationToken {
    fn new(token: TransientEffectToken) -> Self {
        Self { token: Some(token) }
    }

    fn token_mut(&mut self) -> &mut TransientEffectToken {
        self.token
            .as_mut()
            .expect("destination token guard retains its effect token")
    }

    #[cfg(test)]
    fn token(&self) -> &TransientEffectToken {
        self.token
            .as_ref()
            .expect("destination token guard retains its effect token")
    }

    fn into_effect_token(mut self) -> TransientEffectToken {
        self.token
            .take()
            .expect("destination token guard retains its effect token")
    }
}

impl Drop for TransientDestinationToken {
    fn drop(&mut self) {
        if let Some(token) = &self.token {
            token.mark_disposition_on_drop(TransientEffectDisposition::NoEffect);
        }
    }
}

struct DestinationBatchPlan {
    names: Vec<LeafName>,
    targets: HashMap<LeafNameEquivalenceKey, usize>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum DestinationCollisionPolicy {
    RequireVacant,
    AllowExternalCollision,
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

    fn settle_classified_batch(
        members: &mut [ClassifiedPublicationMember],
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        let Some(first) = members.first() else {
            return Err(io::ErrorKind::InvalidInput.into());
        };
        let authority = &first.token().authority;
        for (index, member) in members.iter().enumerate() {
            let token = member.token();
            if !token.armed
                || !Arc::ptr_eq(&token.authority, authority)
                || !Arc::ptr_eq(&token.authority, &operation.authority)
                || members[..index]
                    .iter()
                    .any(|previous| previous.token().id == token.id)
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::ErrorKind::Other)?;
        if state.active == 0 {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        for member in members.iter() {
            let token = member.token();
            let destination = member.destination();
            let record = state
                .transients
                .get(&token.id)
                .ok_or(io::ErrorKind::PermissionDenied)?;
            if record.phase != TransientEffectPhase::Live
                || record.disposition != TransientEffectDisposition::Staged
                || record.identity != Some(member.identity())
                || record.retained.is_some()
                || record.directory.inner.identity != destination.directory.inner.identity
                || record.destination != destination.name
            {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
        }
        let published = members.iter().filter(|member| member.is_published()).count();
        let outstanding_effects = state
            .outstanding_effects
            .checked_sub(published)
            .ok_or(io::ErrorKind::PermissionDenied)?;
        for member in members.iter() {
            if member.is_published() {
                let removed = state.transients.remove(&member.token().id);
                debug_assert!(removed.is_some(), "prechecked transient effect is registered");
            }
        }
        state.outstanding_effects = outstanding_effects;
        drop(state);
        for member in members {
            if member.is_published() {
                member.token_mut().armed = false;
            }
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
    token: Option<TransientDestinationToken>,
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
        if let Err(error) = self.directory.validate(&operation) {
            return TransientStageCreateOutcome::NoEffect {
                error,
                destination: self,
            };
        }
        match platform::create_transient_file(&self.directory.inner.handle) {
            Ok((file, identity)) => {
                let token = self
                    .token
                    .take()
                    .expect("admitted transient destination retains its effect token")
                    .into_effect_token();
                let stage = TransientStage {
                    destination: Some(self),
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
            .expect("admitted transient destination retains its effect token")
            .token_mut();
        match token
            .mark_disposition(TransientEffectDisposition::NoEffect)
            .and_then(|()| token.settle_with(&operation))
        {
            Ok(()) => TransientDestinationCancelOutcome::Cancelled,
            Err(error) => pending_destination_cancel(error, self),
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
            DestinationCollisionPolicy::RequireVacant,
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
                token: Some(TransientDestinationToken::new(token)),
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

#[cfg(target_os = "linux")]
fn enter_publication_reconciliation(
    batch: &mut TransientPublicationBatch,
) -> io::Result<CapabilityOperation> {
    let authority = batch
        .stages
        .first()
        .and_then(|stage| stage.stage.token.as_ref())
        .map(|token| Arc::clone(&token.authority))
        .ok_or(io::ErrorKind::PermissionDenied)?;
    {
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::ErrorKind::Other)?;
        if state.phase != AUTHORITY_LIVE
            && !(state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(&authority))
        {
            return Err(io::ErrorKind::PermissionDenied.into());
        }
        state.active = state.active.checked_add(1).ok_or(io::ErrorKind::Other)?;
    }
    let operation = CapabilityOperation { authority };
    let directory = &batch.directory;
    let directory_buffer = batch.directory_buffer.as_mut_slice();
    platform::validate_lease_preallocated(&operation.authority.lease)?;
    platform::validate_root_preallocated(&operation.authority.root, &mut *directory_buffer)?;
    validate_publication_directory(directory, &operation, Some(directory_buffer))?;
    Ok(operation)
}

#[cfg(not(target_os = "linux"))]
fn enter_publication_reconciliation(
    batch: &mut TransientPublicationBatch,
) -> io::Result<CapabilityOperation> {
    enter_transient_operation(batch.stages[0].stage.destination())
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
    destination: Option<TransientDestination>,
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
    fn destination(&self) -> &TransientDestination {
        self.destination
            .as_ref()
            .expect("live transient stage retains its destination")
    }

    fn take_destination(&mut self) -> TransientDestination {
        self.destination
            .take()
            .expect("live transient stage retains its destination")
    }

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
        let _operation = match enter_transient_operation(self.destination()) {
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
                let destination = self.take_destination();
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
        let destination = self.destination();
        let topology = transient_publication_state_for_publication(
            &file,
            &destination.directory.inner.handle,
            destination.name.as_os_str(),
            self.identity,
        );
        #[cfg(target_os = "linux")]
        let discard = platform::discard_transient_file_preallocated(file, self.identity);
        #[cfg(not(target_os = "linux"))]
        let discard = platform::discard_transient_file(file, self.identity);
        match discard {
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

#[cfg(all(test, target_os = "linux"))]
fn validate_linked_publication(
    sealed: &TransientStageSealed,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let destination = sealed.stage.destination();
    destination.directory.validate(operation)?;
    let file = sealed
        .stage
        .file
        .as_ref()
        .expect("linked transient stage retains its file");
    validate_exact_destination(destination, file, sealed.stage.identity, operation)
}

#[cfg(all(test, target_os = "linux"))]
fn validate_exact_destination(
    destination: &TransientDestination,
    retained: &platform::TransientFile,
    identity: platform::Identity,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    validate_exact_destination_binding(destination, retained, identity, None, operation)?;
    if !validate_portable_destination_with_operation(destination, false, operation)? {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transient publication did not retain its unique exact destination",
        ));
    }
    destination.directory.validate(operation)
}

fn validate_exact_destination_binding(
    destination: &TransientDestination,
    retained: &platform::TransientFile,
    identity: platform::Identity,
    directory_buffer: Option<&mut [MaybeUninit<u8>]>,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let mut directory_buffer = directory_buffer;
    validate_publication_directory(
        &destination.directory,
        operation,
        directory_buffer.as_deref_mut(),
    )?;
    if transient_file_evidence_for_publication(retained)? != (identity, 1)
        || platform::file_binding_state(
            &destination.directory.inner.handle,
            destination.name.as_os_str(),
            identity,
        )? != platform::BindingState::Exact
    {
        return Err(io::ErrorKind::InvalidData.into());
    }
    validate_publication_directory(&destination.directory, operation, directory_buffer)
}

fn validate_unpublished_destination(
    destination: &TransientDestination,
    retained: &platform::TransientFile,
    identity: platform::Identity,
    directory_buffer: Option<&mut [MaybeUninit<u8>]>,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let mut directory_buffer = directory_buffer;
    validate_publication_directory(
        &destination.directory,
        operation,
        directory_buffer.as_deref_mut(),
    )?;
    let binding = platform::file_binding_state(
        &destination.directory.inner.handle,
        destination.name.as_os_str(),
        identity,
    )?;
    if transient_file_evidence_for_publication(retained)? != (identity, 0)
        || binding == platform::BindingState::Exact
    {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    validate_publication_directory(&destination.directory, operation, directory_buffer)
}

fn validate_publication_directory(
    directory: &Directory,
    operation: &CapabilityOperation,
    directory_buffer: Option<&mut [MaybeUninit<u8>]>,
) -> io::Result<()> {
    #[cfg(target_os = "linux")]
    if let Some(buffer) = directory_buffer {
        let mut current = directory.inner.as_ref();
        loop {
            if current.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
                return Err(io::ErrorKind::PermissionDenied.into());
            }
            if platform::directory_identity_preallocated(&current.handle)?
                != current.identity.physical
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            if let Some(ancestry) = &current.absolute_ancestry {
                platform::validate_absolute_directory_guard_preallocated(ancestry, buffer)?;
            }
            let Some(binding) = &current.parent else {
                break;
            };
            if platform::directory_binding_state(
                &binding.directory.inner.handle,
                &binding.name,
                current.identity.physical,
            )? != platform::BindingState::Exact
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
            current = binding.directory.inner.as_ref();
        }
        return Ok(());
    }
    #[cfg(not(target_os = "linux"))]
    debug_assert!(directory_buffer.is_none());
    directory.validate(operation)
}

fn directory_revision_for_publication(
    directory: &Directory,
    directory_buffer: Option<&mut [MaybeUninit<u8>]>,
) -> io::Result<platform::DirectoryStamp> {
    #[cfg(target_os = "linux")]
    if directory_buffer.is_some() {
        return platform::directory_revision_preallocated(&directory.inner.handle);
    }
    #[cfg(not(target_os = "linux"))]
    debug_assert!(directory_buffer.is_none());
    platform::directory_revision(&directory.inner.handle)
}

fn transient_publication_state_for_publication(
    transient: &platform::TransientFile,
    parent: &platform::DirectoryHandle,
    destination_name: &std::ffi::OsStr,
    expected: platform::Identity,
) -> io::Result<platform::TransientPublicationState> {
    #[cfg(target_os = "linux")]
    return platform::transient_publication_state_preallocated(
        transient,
        parent,
        destination_name,
        expected,
    );
    #[cfg(not(target_os = "linux"))]
    platform::transient_publication_state(transient, parent, destination_name, expected)
}

fn transient_file_evidence_for_publication(
    transient: &platform::TransientFile,
) -> io::Result<(platform::Identity, u64)> {
    #[cfg(target_os = "linux")]
    return platform::transient_file_evidence_preallocated(transient);
    #[cfg(not(target_os = "linux"))]
    platform::transient_file_evidence(transient)
}

fn validate_portable_destination_with_operation(
    destination: &TransientDestination,
    require_vacant: bool,
    operation: &CapabilityOperation,
) -> io::Result<bool> {
    let plan = DestinationBatchPlan::new(vec![destination.name.clone()])?;
    let collision_policy = if require_vacant {
        DestinationCollisionPolicy::RequireVacant
    } else {
        DestinationCollisionPolicy::AllowExternalCollision
    };
    let exact = validate_destination_batch_with_operation(
        &destination.directory,
        &plan,
        collision_policy,
        operation,
    )?;
    Ok(exact[0])
}

fn validate_destination_batch_with_operation(
    directory: &Directory,
    plan: &DestinationBatchPlan,
    collision_policy: DestinationCollisionPolicy,
    operation: &CapabilityOperation,
) -> io::Result<Vec<bool>> {
    let expected_exact = vec![
        collision_policy == DestinationCollisionPolicy::AllowExternalCollision;
        plan.names.len()
    ];
    let mut exact = vec![false; plan.names.len()];
    let mut portable_key = Vec::new();
    let mut native_key = Vec::new();
    let mut normalization = Vec::new();
    portable_key
        .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("portable leaf proof capacity is exhausted"))?;
    native_key
        .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("native leaf proof capacity is exhausted"))?;
    normalization
        .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("leaf normalization capacity is exhausted"))?;
    validate_mixed_destination_batch_with_operation(
        directory,
        plan,
        &expected_exact,
        &mut exact,
        &mut portable_key,
        &mut native_key,
        &mut normalization,
        collision_policy,
        None,
        operation,
    )?;
    Ok(exact)
}

fn validate_mixed_destination_batch_with_operation(
    directory: &Directory,
    plan: &DestinationBatchPlan,
    expected_exact: &[bool],
    exact: &mut [bool],
    portable_key: &mut Vec<u8>,
    native_key: &mut Vec<u8>,
    normalization: &mut Vec<(u8, char)>,
    collision_policy: DestinationCollisionPolicy,
    mut directory_buffer: Option<&mut [MaybeUninit<u8>]>,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    if expected_exact.len() != plan.names.len() || exact.len() != plan.names.len() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    exact.fill(false);
    validate_publication_directory(directory, operation, directory_buffer.as_deref_mut())?;
    let revision_before =
        directory_revision_for_publication(directory, directory_buffer.as_deref_mut())?;
    let mut conflict: Option<io::Error> = None;
    let mut visit_entry = |observed_name: &std::ffi::OsStr, kind| {
        let has_portable = platform::fill_leaf_name_equivalence_keys(
            observed_name,
            portable_key,
            native_key,
            normalization,
        )?;
        let portable_target = has_portable
            .then(|| plan.targets.get(portable_key.as_slice()).copied())
            .flatten();
        let native_target = plan.targets.get(native_key.as_slice()).copied();
        if let (Some(portable_target), Some(native_target)) =
            (portable_target, native_target)
        {
            if portable_target != native_target {
                if collision_policy == DestinationCollisionPolicy::AllowExternalCollision
                    && !expected_exact[portable_target]
                    && !expected_exact[native_target]
                {
                    return Ok(ControlFlow::Continue(()));
                }
                conflict = Some(io::ErrorKind::AlreadyExists.into());
                return Ok(ControlFlow::Break(()));
            }
        }
        let target = portable_target.or(native_target);
        let Some(target) = target else {
            return Ok(ControlFlow::Continue(()));
        };
        if collision_policy == DestinationCollisionPolicy::RequireVacant {
            conflict = Some(io::ErrorKind::AlreadyExists.into());
            return Ok(ControlFlow::Break(()));
        }
        if !expected_exact[target] {
            return Ok(ControlFlow::Continue(()));
        }
        let target_name = plan.names[target].as_os_str();
        let error = if observed_name != target_name || exact[target] {
            Some(io::ErrorKind::AlreadyExists.into())
        } else if kind != EntryKind::File {
            Some(io::ErrorKind::AlreadyExists.into())
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
    };
    #[cfg(target_os = "linux")]
    let visit = match directory_buffer.as_deref_mut() {
        Some(buffer) => platform::visit_entries_preallocated(
            &directory.inner.handle,
            buffer,
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        ),
        None => platform::visit_entries(
            &directory.inner.handle,
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        ),
    };
    #[cfg(not(target_os = "linux"))]
    let visit = {
        debug_assert!(directory_buffer.is_none());
        platform::visit_entries(
            &directory.inner.handle,
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        )
    };
    validate_publication_directory(directory, operation, directory_buffer.as_deref_mut())?;
    let revision_after =
        directory_revision_for_publication(directory, directory_buffer.as_deref_mut())?;
    let completion = visit?;
    if revision_after != revision_before {
        return Err(io::ErrorKind::WouldBlock.into());
    }
    // Equal stamps never replace the complete inventory; they only fail a
    // proof when an observable namespace revision changed around the scan.
    match completion {
        platform::VisitCompletion::Complete => Ok(()),
        platform::VisitCompletion::Stopped => Err(conflict.expect(
            "transient destination inventory stops only for a decisive conflict",
        )),
        platform::VisitCompletion::LimitExceeded => Err(io::ErrorKind::InvalidData.into()),
    }
}

/// A bounded monotonic publication group.
///
/// This operation is intentionally not an atomic visibility transaction. It
/// reports all published members, zero published members, or a fully classified
/// ordered mix without deleting any name that may have become visible. Callers
/// that require all-or-none visibility must publish inside a private capability-
/// owned directory or generation and commit that container separately.
/// On the supported Linux native path, classification and reconciliation after
/// the first visible link use only storage reserved by batch construction.
#[must_use = "transient publication batches must be published or explicitly discarded"]
pub struct TransientPublicationBatch {
    directory: Directory,
    plan: DestinationBatchPlan,
    stages: Vec<TransientStageSealed>,
    classifications: Vec<bool>,
    inventory_exact: Vec<bool>,
    portable_key: Vec<u8>,
    native_key: Vec<u8>,
    normalization: Vec<(u8, char)>,
    #[cfg(target_os = "linux")]
    directory_buffer: Vec<MaybeUninit<u8>>,
    classified: Vec<ClassifiedPublicationMember>,
    published_output: Vec<FileCapability>,
    output: Vec<TransientPublicationMember>,
}

impl std::fmt::Debug for TransientPublicationBatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationBatch")
            .field("len", &self.stages.len())
            .finish_non_exhaustive()
    }
}

#[must_use = "refused transient publication batches retain every sealed stage"]
pub struct TransientPublicationBatchCreateFailure {
    error: io::Error,
    stages: Option<Vec<TransientStageSealed>>,
}

impl std::fmt::Debug for TransientPublicationBatchCreateFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationBatchCreateFailure")
            .finish_non_exhaustive()
    }
}

impl TransientPublicationBatchCreateFailure {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_stages(mut self) -> Vec<TransientStageSealed> {
        self.stages
            .take()
            .expect("publication batch creation failure retains every stage")
    }
}

#[must_use = "transient publication outcomes retain every unsettled effect"]
pub enum TransientPublicationBatchOutcome {
    /// Every member is durably bound to its exact destination.
    Published(Vec<FileCapability>),
    /// No member was published; the intact ordered batch remains retryable.
    NoEffect {
        error: io::Error,
        batch: TransientPublicationBatch,
    },
    /// At least one member published and at least one remained unpublished.
    Partial {
        error: io::Error,
        members: Vec<TransientPublicationMember>,
    },
    /// At least one member could not be classified conclusively.
    Pending(TransientPublicationBatchObligation),
}

#[must_use = "partial transient publication members retain exact filesystem authority"]
pub enum TransientPublicationMember {
    Published(FileCapability),
    Unpublished(TransientStageSealed),
}

impl std::fmt::Debug for TransientPublicationBatchOutcome {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationBatchOutcome")
            .finish_non_exhaustive()
    }
}

struct TransientPublicationTransition {
    stage: Option<TransientStageSealed>,
    identity: platform::Identity,
    retained: Option<platform::TransientFile>,
    token: Option<TransientEffectToken>,
}

impl TransientPublicationTransition {
    fn from_linked(mut stage: TransientStageSealed) -> Self {
        let identity = stage.stage.identity;
        let mut transition = Self {
            stage: Some(stage),
            identity,
            retained: None,
            token: None,
        };
        transition.extract_retained();
        transition.extract_token();
        transition
    }

    fn stage_mut(&mut self) -> &mut TransientStageSealed {
        self.stage
            .as_mut()
            .expect("publication transition retains its sealed stage")
    }

    fn extract_retained(&mut self) {
        assert!(
            self.retained.is_none(),
            "publication transition extracted duplicate native authority"
        );
        self.retained = self.stage_mut().stage.file.take();
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
        self.token = self.stage_mut().stage.token.take();
        assert!(
            self.token.is_some(),
            "linked transient stage retains its effect token"
        );
    }

    fn destination(&self) -> &TransientDestination {
        self.stage
            .as_ref()
            .expect("publication transition retains its destination")
            .stage
            .destination()
    }

    fn into_stage(mut self) -> TransientStageSealed {
        if self.stage_mut().stage.file.is_none() {
            let retained = self.retained.take();
            self.stage_mut().stage.file = retained;
        }
        if self.stage_mut().stage.token.is_none() {
            let token = self.token.take();
            self.stage_mut().stage.token = token;
        }
        self
            .stage
            .take()
            .expect("publication transition retains its sealed stage")
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
        let authority = Arc::downgrade(
            &self
                .token
                .as_ref()
                .expect("publication transition retains its effect token")
                .authority,
        );
        let mut stage = self
            .stage
            .take()
            .expect("publication transition retains its sealed stage");
        let destination = stage.stage.take_destination();
        let TransientDestination {
            directory,
            name,
            token,
        } = destination;
        assert!(
            token.is_none(),
            "live transient stage destination does not retain duplicate effect authority"
        );
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
            .or(self
                .stage
                .as_ref()
                .and_then(|stage| stage.stage.token.as_ref()))
            .is_some_and(|token| token.armed);
        if !armed {
            drop(self.retained.take());
            if let Some(stage) = self.stage.as_mut() {
                drop(stage.stage.file.take());
            }
            return;
        }
        if let Some(stage) = self.stage.as_mut() {
            if stage.stage.file.is_none() {
                stage.stage.file = self.retained.take();
            }
            if stage.stage.token.is_none() {
                stage.stage.token = self.token.take();
            }
        }
    }
}

enum ClassifiedPublicationMember {
    Published(TransientPublicationTransition),
    Unpublished(TransientStageSealed),
}

impl ClassifiedPublicationMember {
    fn is_published(&self) -> bool {
        matches!(self, Self::Published(_))
    }

    fn token(&self) -> &TransientEffectToken {
        match self {
            Self::Published(transition) => transition
                .token
                .as_ref()
                .expect("published member retains its effect token"),
            Self::Unpublished(stage) => stage
                .stage
                .token
                .as_ref()
                .expect("unpublished member retains its effect token"),
        }
    }

    fn token_mut(&mut self) -> &mut TransientEffectToken {
        match self {
            Self::Published(transition) => transition
                .token
                .as_mut()
                .expect("published member retains its effect token"),
            Self::Unpublished(stage) => stage
                .stage
                .token
                .as_mut()
                .expect("unpublished member retains its effect token"),
        }
    }

    fn destination(&self) -> &TransientDestination {
        match self {
            Self::Published(transition) => transition.destination(),
            Self::Unpublished(stage) => stage.stage.destination(),
        }
    }

    fn identity(&self) -> platform::Identity {
        match self {
            Self::Published(transition) => transition.identity,
            Self::Unpublished(stage) => stage.stage.identity,
        }
    }
}

#[must_use = "pending transient publication authority must be reconciled"]
pub struct TransientPublicationBatchObligation {
    error: io::Error,
    batch: Option<TransientPublicationBatch>,
}

impl std::fmt::Debug for TransientPublicationBatchObligation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TransientPublicationBatchObligation")
            .finish_non_exhaustive()
    }
}

impl TransientPublicationBatch {
    pub fn new(
        stages: Vec<TransientStageSealed>,
    ) -> Result<Self, TransientPublicationBatchCreateFailure> {
        if stages.is_empty() || stages.len() > MAX_OUTSTANDING_EFFECTS {
            return Err(TransientPublicationBatchCreateFailure {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "transient publication batch size is outside the supported range",
                ),
                stages: Some(stages),
            });
        }
        let directory = stages[0].stage.destination().directory.clone();
        if stages.iter().any(|stage| {
            !std::sync::Weak::ptr_eq(
                &stage.stage.destination().directory.inner.authority,
                &directory.inner.authority,
            ) || stage.stage.destination().directory.inner.identity != directory.inner.identity
        }) {
            return Err(TransientPublicationBatchCreateFailure {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "transient publication batch spans filesystem authorities or physical parents",
                ),
                stages: Some(stages),
            });
        }
        let names = stages
            .iter()
            .map(|stage| stage.stage.destination().name.clone())
            .collect();
        let plan = match DestinationBatchPlan::new(names) {
            Ok(plan) => plan,
            Err(error) => {
                return Err(TransientPublicationBatchCreateFailure {
                    error,
                    stages: Some(stages),
                });
            }
        };
        let mut classifications = Vec::new();
        let mut inventory_exact = Vec::new();
        let mut portable_key = Vec::new();
        let mut native_key = Vec::new();
        let mut normalization = Vec::new();
        #[cfg(target_os = "linux")]
        let mut directory_buffer = Vec::new();
        #[cfg(target_os = "linux")]
        let directory_buffer_exhausted = directory_buffer
            .try_reserve_exact(platform::TRANSIENT_DIRECTORY_BUFFER_BYTES)
            .is_err();
        #[cfg(not(target_os = "linux"))]
        let directory_buffer_exhausted = false;
        let mut classified = Vec::new();
        let mut published_output = Vec::new();
        let mut output = Vec::new();
        if classifications.try_reserve_exact(stages.len()).is_err()
            || inventory_exact.try_reserve_exact(stages.len()).is_err()
            || portable_key
                .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
                .is_err()
            || native_key
                .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
                .is_err()
            || normalization
                .try_reserve_exact(MAX_TRANSIENT_EQUIVALENCE_KEY_BYTES)
                .is_err()
            || directory_buffer_exhausted
            || classified.try_reserve_exact(stages.len()).is_err()
            || published_output.try_reserve_exact(stages.len()).is_err()
            || output.try_reserve_exact(stages.len()).is_err()
        {
            return Err(TransientPublicationBatchCreateFailure {
                error: io::Error::other(
                    "transient publication batch working capacity is exhausted",
                ),
                stages: Some(stages),
            });
        }
        inventory_exact.resize(stages.len(), false);
        #[cfg(target_os = "linux")]
        directory_buffer.resize_with(
            platform::TRANSIENT_DIRECTORY_BUFFER_BYTES,
            MaybeUninit::uninit,
        );
        Ok(Self {
            directory,
            plan,
            stages,
            classifications,
            inventory_exact,
            portable_key,
            native_key,
            normalization,
            #[cfg(target_os = "linux")]
            directory_buffer,
            classified,
            published_output,
            output,
        })
    }

    pub fn len(&self) -> usize {
        self.stages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.stages.is_empty()
    }

    pub fn into_stages(self) -> Vec<TransientStageSealed> {
        self.stages
    }

    pub fn publish_create_new(mut self) -> TransientPublicationBatchOutcome {
        let authority = match self.directory.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return TransientPublicationBatchOutcome::NoEffect { error, batch: self };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return TransientPublicationBatchOutcome::NoEffect { error, batch: self };
            }
        };
        if let Err(error) = validate_destination_batch_with_operation(
            &self.directory,
            &self.plan,
            DestinationCollisionPolicy::RequireVacant,
            &operation,
        ) {
            return TransientPublicationBatchOutcome::NoEffect { error, batch: self };
        }
        for index in 0..self.stages.len() {
            let stage = &mut self.stages[index];
            let TransientStage {
                destination,
                file,
                identity,
                ..
            } = &mut stage.stage;
            let destination = destination
                .as_ref()
                .expect("sealed transient stage retains its destination");
            let file = file
                .as_mut()
                .expect("sealed transient stage retains its file");
            let link = platform::link_transient_file(
                file,
                &destination.directory.inner.handle,
                destination.name.as_os_str(),
            );
            let state = transient_publication_state_for_publication(
                file,
                &destination.directory.inner.handle,
                destination.name.as_os_str(),
                *identity,
            );
            match (link, state) {
                (Ok(()), Ok(platform::TransientPublicationState::Published)) => {}
                (Err(error), Ok(platform::TransientPublicationState::Unpublished))
                    if index == 0 =>
                {
                    return TransientPublicationBatchOutcome::NoEffect { error, batch: self };
                }
                (Err(error), _) => {
                    return classify_publication_batch(error, self, &operation);
                }
                (Ok(()), _) => {
                    return pending_publication(
                        io::ErrorKind::InvalidData.into(),
                        self,
                    );
                }
            }
        }
        classify_publication_batch(
            io::ErrorKind::Other.into(),
            self,
            &operation,
        )
    }
}

fn classify_publication_batch(
    error: io::Error,
    mut batch: TransientPublicationBatch,
    operation: &CapabilityOperation,
) -> TransientPublicationBatchOutcome {
    batch.classifications.clear();
    for stage in &batch.stages {
        let file = stage
            .stage
            .file
            .as_ref()
            .expect("sealed transient stage retains its file");
        let destination = stage.stage.destination();
        match transient_publication_state_for_publication(
            file,
            &destination.directory.inner.handle,
            destination.name.as_os_str(),
            stage.stage.identity,
        ) {
            Ok(platform::TransientPublicationState::Published) => {
                batch.classifications.push(true);
            }
            Ok(platform::TransientPublicationState::Unpublished) => {
                batch.classifications.push(false);
            }
            Ok(platform::TransientPublicationState::Indeterminate) => {
                return pending_publication(
                    io::ErrorKind::WouldBlock.into(),
                    batch,
                );
            }
            Err(classification) => return pending_publication(classification, batch),
        }
    }
    #[cfg(target_os = "linux")]
    let member_proof = validate_classified_publication_members(
        &batch.stages,
        &batch.classifications,
        Some(batch.directory_buffer.as_mut_slice()),
        operation,
    );
    #[cfg(not(target_os = "linux"))]
    let member_proof = validate_classified_publication_members(
        &batch.stages,
        &batch.classifications,
        None,
        operation,
    );
    if let Err(proof) = member_proof {
        return pending_publication(proof, batch);
    }
    if let Err(sync) = platform::sync_directory(&batch.directory.inner.handle) {
        return pending_publication(sync, batch);
    }
    #[cfg(target_os = "linux")]
    let directory_buffer = Some(batch.directory_buffer.as_mut_slice());
    #[cfg(not(target_os = "linux"))]
    let directory_buffer = None;
    if let Err(proof) = validate_mixed_destination_batch_with_operation(
        &batch.directory,
        &batch.plan,
        &batch.classifications,
        &mut batch.inventory_exact,
        &mut batch.portable_key,
        &mut batch.native_key,
        &mut batch.normalization,
        DestinationCollisionPolicy::AllowExternalCollision,
        directory_buffer,
        operation,
    ) {
        return pending_publication(proof, batch);
    }
    if batch.inventory_exact != batch.classifications {
        return pending_publication(
            io::ErrorKind::InvalidData.into(),
            batch,
        );
    }
    #[cfg(target_os = "linux")]
    let member_proof = validate_classified_publication_members(
        &batch.stages,
        &batch.classifications,
        Some(batch.directory_buffer.as_mut_slice()),
        operation,
    );
    #[cfg(not(target_os = "linux"))]
    let member_proof = validate_classified_publication_members(
        &batch.stages,
        &batch.classifications,
        None,
        operation,
    );
    if let Err(proof) = member_proof {
        return pending_publication(proof, batch);
    }
    let published_count = batch
        .classifications
        .iter()
        .filter(|published| **published)
        .count();
    if published_count == 0 {
        return TransientPublicationBatchOutcome::NoEffect { error, batch };
    }
    let all_published = published_count == batch.classifications.len();
    let TransientPublicationBatch {
        directory,
        plan,
        mut stages,
        classifications,
        inventory_exact,
        portable_key,
        native_key,
        normalization,
        #[cfg(target_os = "linux")]
        directory_buffer,
        mut classified,
        mut published_output,
        mut output,
    } = batch;
    for (stage, published) in stages
        .drain(..)
        .zip(classifications.iter().copied())
    {
        if published {
            classified.push(ClassifiedPublicationMember::Published(
                TransientPublicationTransition::from_linked(stage),
            ));
        } else {
            classified.push(ClassifiedPublicationMember::Unpublished(stage));
        }
    }
    if let Err(settlement) = TransientEffectToken::settle_classified_batch(
        &mut classified,
        operation,
    ) {
        for member in classified.drain(..) {
            match member {
                ClassifiedPublicationMember::Published(transition) => {
                    stages.push(transition.into_stage());
                }
                ClassifiedPublicationMember::Unpublished(stage) => {
                    stages.push(stage);
                }
            }
        }
        return pending_publication(
            settlement,
            TransientPublicationBatch {
                directory,
                plan,
                stages,
                classifications,
                inventory_exact,
                portable_key,
                native_key,
                normalization,
                #[cfg(target_os = "linux")]
                directory_buffer,
                classified,
                published_output,
                output,
            },
        );
    }
    if all_published {
        for member in classified.drain(..) {
            let ClassifiedPublicationMember::Published(transition) = member else {
                unreachable!("complete publication classified every member as published");
            };
            published_output.push(transition.into_file_capability());
        }
        return TransientPublicationBatchOutcome::Published(published_output);
    }
    for member in classified.drain(..) {
        match member {
            ClassifiedPublicationMember::Published(transition) => {
                output.push(TransientPublicationMember::Published(
                    transition.into_file_capability(),
                ));
            }
            ClassifiedPublicationMember::Unpublished(stage) => {
                output.push(TransientPublicationMember::Unpublished(stage));
            }
        }
    }
    TransientPublicationBatchOutcome::Partial {
        error,
        members: output,
    }
}

fn pending_publication(
    error: io::Error,
    batch: TransientPublicationBatch,
) -> TransientPublicationBatchOutcome {
    TransientPublicationBatchOutcome::Pending(TransientPublicationBatchObligation {
        error,
        batch: Some(batch),
    })
}

fn validate_classified_publication_members(
    stages: &[TransientStageSealed],
    classifications: &[bool],
    mut directory_buffer: Option<&mut [MaybeUninit<u8>]>,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    for (stage, published) in stages.iter().zip(classifications) {
        let file = stage
            .stage
            .file
            .as_ref()
            .expect("classified transient stage retains its file");
        if *published {
            validate_exact_destination_binding(
                stage.stage.destination(),
                file,
                stage.stage.identity,
                directory_buffer.as_deref_mut(),
                operation,
            )?;
        } else {
            validate_unpublished_destination(
                stage.stage.destination(),
                file,
                stage.stage.identity,
                directory_buffer.as_deref_mut(),
                operation,
            )?;
        }
    }
    Ok(())
}

impl TransientPublicationBatchObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> TransientPublicationBatchOutcome {
        let error = std::mem::replace(
            &mut self.error,
            io::ErrorKind::Other.into(),
        );
        let mut batch = self
            .batch
            .take()
            .expect("publication obligation retains its batch");
        let operation = match enter_publication_reconciliation(&mut batch) {
            Ok(operation) => operation,
            Err(reconcile) => return pending_publication(reconcile, batch),
        };
        classify_publication_batch(error, batch, &operation)
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
            destination.token = Some(TransientDestinationToken::new(token));
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
                .token()
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
        let publication_pending = TransientPublicationBatchObligation {
            error: io::Error::other("injected publication settlement"),
            batch: Some(
                TransientPublicationBatch::new(vec![
                    publication_stage.seal().expect("sealed publication stage"),
                ])
                .expect("singleton publication batch"),
            ),
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
        let batch = TransientPublicationBatch::new(vec![sealed])
            .expect("singleton publication batch");
        let published = match batch.publish_create_new() {
            TransientPublicationBatchOutcome::Published(mut files) => {
                files.pop().expect("singleton published file")
            }
            TransientPublicationBatchOutcome::NoEffect { error, .. } => {
                panic!("publication had no effect: {error}")
            }
            TransientPublicationBatchOutcome::Partial { error, .. } => {
                panic!("publication was partial: {error}")
            }
            TransientPublicationBatchOutcome::Pending(obligation) => {
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
    fn grouped_publication_releases_every_file_after_one_terminal_outcome() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let mut first = test_stage(&root, "group-first.bin").expect("first transient stage");
        let mut second = test_stage(&root, "group-second.bin").expect("second transient stage");
        first.write_all(b"first").expect("first stage write");
        second.write_all(b"second").expect("second stage write");
        let batch = TransientPublicationBatch::new(vec![
            first.seal().expect("first stage seal"),
            second.seal().expect("second stage seal"),
        ])
        .expect("grouped publication batch");
        let files = match batch.publish_create_new() {
            TransientPublicationBatchOutcome::Published(files) => files,
            TransientPublicationBatchOutcome::NoEffect { error, .. } => {
                panic!("grouped publication had no effect: {error}")
            }
            TransientPublicationBatchOutcome::Partial { error, .. } => {
                panic!("grouped publication was partial: {error}")
            }
            TransientPublicationBatchOutcome::Pending(obligation) => {
                panic!("grouped publication remained pending: {}", obligation.error())
            }
        };
        assert_eq!(files.len(), 2);
        assert_eq!(
            std::fs::read(temporary.path().join("group-first.bin"))
                .expect("first published payload"),
            b"first",
        );
        assert_eq!(
            std::fs::read(temporary.path().join("group-second.bin"))
                .expect("second published payload"),
            b"second",
        );
        drop(files);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_partial_publication_preserves_original_member_order() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let first = test_stage(&root, "partial-first.bin").expect("first transient stage");
        let second = test_stage(&root, "partial-second.bin").expect("second transient stage");
        let stages = vec![
            first.seal().expect("first stage seal"),
            second.seal().expect("second stage seal"),
        ];
        let mut batch = TransientPublicationBatch::new(stages)
            .expect("partial grouped publication batch");
        let first_id = batch.stages[0]
            .stage
            .token
            .as_ref()
            .expect("first stage effect token")
            .id;
        let second_id = batch.stages[1]
            .stage
            .token
            .as_ref()
            .expect("second stage effect token")
            .id;
        platform::link_transient_file(
            batch.stages[0]
                .stage
                .file
                .as_mut()
                .expect("first stage retains its native file"),
            &root.inner.handle,
            OsStr::new("partial-first.bin"),
        )
        .expect("partial grouped publication");
        std::fs::write(
            temporary.path().join("partial-second.bin"),
            b"external collision",
        )
        .expect("stable external collision");
        let operation = enter_transient_operation(batch.stages[0].stage.destination())
            .expect("partial publication operation");
        let members = match classify_publication_batch(
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "injected partial grouped publication collision",
            ),
            batch,
            &operation,
        ) {
            TransientPublicationBatchOutcome::Partial { members, .. } => members,
            TransientPublicationBatchOutcome::Pending(obligation) => {
                panic!("grouped partial remained pending: {}", obligation.error())
            }
            _ => panic!("grouped publication was not classified as partial"),
        };
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("partial publication state");
            assert!(!state.transients.contains_key(&first_id));
            let unpublished = state
                .transients
                .get(&second_id)
                .expect("unpublished reservation remains live");
            assert!(unpublished.phase == TransientEffectPhase::Live);
            assert!(unpublished.disposition == TransientEffectDisposition::Staged);
            assert!(unpublished.retained.is_none());
            assert_eq!(state.outstanding_effects, 1);
        }
        let mut members = members.into_iter();
        match members.next().expect("first classified member") {
            TransientPublicationMember::Published(file) => drop(file),
            TransientPublicationMember::Unpublished(_) => {
                panic!("first classified member lost its published position")
            }
        }
        let stage = match members.next().expect("second classified member") {
            TransientPublicationMember::Published(_) => {
                panic!("second classified member lost its unpublished position")
            }
            TransientPublicationMember::Unpublished(stage) => stage,
        };
        assert!(members.next().is_none());
        let destination = match stage.discard() {
            TransientDiscardOutcome::Discarded(destination) => destination,
            TransientDiscardOutcome::Pending(obligation) => {
                panic!("partial stage discard remained pending: {}", obligation.error())
            }
        };
        match destination.cancel() {
            TransientDestinationCancelOutcome::Cancelled => {}
            TransientDestinationCancelOutcome::Pending(obligation) => {
                panic!("partial destination cancellation remained pending: {}", obligation.error())
            }
        }
        assert!(temporary.path().join("partial-first.bin").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("partial-second.bin"))
                .expect("external collision remains"),
            b"external collision",
        );
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_partial_accepts_stable_unpublished_alias_and_wrong_kind_collisions() {
        for (case, target_name, collision_name, collision_is_directory) in [
            (
                "portable-alias",
                "portable-target.bin",
                "PORTABLE-TARGET.BIN",
                false,
            ),
            (
                "wrong-kind",
                "wrong-kind-target.bin",
                "wrong-kind-target.bin",
                true,
            ),
        ] {
            let temporary = tempfile::tempdir().expect("temporary transient root");
            let session = acquire_test_root(temporary.path());
            let root = session.root().expect("root directory");
            let first_name = format!("{case}-published.bin");
            let first = test_stage(&root, &first_name).expect("first transient stage");
            let second = test_stage(&root, target_name).expect("second transient stage");
            let mut batch = TransientPublicationBatch::new(vec![
                first.seal().expect("first stage seal"),
                second.seal().expect("second stage seal"),
            ])
            .expect("collision publication batch");
            platform::link_transient_file(
                batch.stages[0]
                    .stage
                    .file
                    .as_mut()
                    .expect("first stage native file"),
                &root.inner.handle,
                OsStr::new(first_name.as_str()),
            )
            .expect("published prefix");
            let collision_path = temporary.path().join(collision_name);
            if collision_is_directory {
                std::fs::create_dir(&collision_path).expect("wrong-kind external collision");
            } else {
                std::fs::write(&collision_path, b"portable alias collision")
                    .expect("portable alias external collision");
            }
            let operation = enter_transient_operation(batch.stages[0].stage.destination())
                .expect("collision classification operation");
            let members = match classify_publication_batch(
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "injected stable external collision",
                ),
                batch,
                &operation,
            ) {
                TransientPublicationBatchOutcome::Partial { members, .. } => members,
                TransientPublicationBatchOutcome::Pending(obligation) => {
                    panic!("stable collision remained pending: {}", obligation.error())
                }
                _ => panic!("stable collision was not classified as partial"),
            };
            let mut members = members.into_iter();
            match members.next().expect("published prefix member") {
                TransientPublicationMember::Published(file) => drop(file),
                TransientPublicationMember::Unpublished(_) => {
                    panic!("published prefix lost its ordered classification")
                }
            }
            let stage = match members.next().expect("unpublished collision member") {
                TransientPublicationMember::Published(_) => {
                    panic!("external collision was classified as published")
                }
                TransientPublicationMember::Unpublished(stage) => stage,
            };
            assert!(members.next().is_none());
            let destination = match stage.discard() {
                TransientDiscardOutcome::Discarded(destination) => destination,
                TransientDiscardOutcome::Pending(obligation) => {
                    panic!("collision stage discard remained pending: {}", obligation.error())
                }
            };
            match destination.cancel() {
                TransientDestinationCancelOutcome::Cancelled => {}
                TransientDestinationCancelOutcome::Pending(obligation) => {
                    panic!("collision cancellation remained pending: {}", obligation.error())
                }
            }
            assert!(collision_path.exists());
            drop(root);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropped_mixed_pending_batch_retains_root_cleanable_authority() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let first = test_stage(&root, "pending-first.bin").expect("first transient stage");
        let second = test_stage(&root, "pending-second.bin").expect("second transient stage");
        let mut batch = TransientPublicationBatch::new(vec![
            first.seal().expect("first stage seal"),
            second.seal().expect("second stage seal"),
        ])
        .expect("mixed pending batch");
        let first_id = batch.stages[0]
            .stage
            .token
            .as_ref()
            .expect("first stage effect token")
            .id;
        let second_id = batch.stages[1]
            .stage
            .token
            .as_ref()
            .expect("second stage effect token")
            .id;
        platform::link_transient_file(
            batch.stages[0]
                .stage
                .file
                .as_mut()
                .expect("first stage native file"),
            &root.inner.handle,
            OsStr::new("pending-first.bin"),
        )
        .expect("published prefix");
        drop(TransientPublicationBatchObligation {
            error: io::Error::other("injected mixed pending classification"),
            batch: Some(batch),
        });

        assert_retained_transient(&session, first_id, TransientEffectDisposition::Published);
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("dropped mixed pending state");
            let unpublished = state
                .transients
                .get(&second_id)
                .expect("unpublished effect remains root owned");
            assert!(unpublished.phase == TransientEffectPhase::Abandoned);
            assert!(unpublished.disposition == TransientEffectDisposition::NoEffect);
            assert!(unpublished.retained.is_none());
            assert_eq!(state.outstanding_effects, 2);
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_zero_publication_returns_the_intact_no_effect_batch() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let first = test_stage(&root, "zero-first.bin").expect("first transient stage");
        let second = test_stage(&root, "zero-second.bin").expect("second transient stage");
        let batch = TransientPublicationBatch::new(vec![
            first.seal().expect("first stage seal"),
            second.seal().expect("second stage seal"),
        ])
        .expect("zero-publication batch");
        std::fs::write(temporary.path().join("zero-first.bin"), b"first collision")
            .expect("first external collision");
        std::fs::write(temporary.path().join("zero-second.bin"), b"second collision")
            .expect("second external collision");
        let operation = enter_transient_operation(batch.stages[0].stage.destination())
            .expect("zero-publication operation");
        let batch = match classify_publication_batch(
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "injected zero-publication collision",
            ),
            batch,
            &operation,
        ) {
            TransientPublicationBatchOutcome::NoEffect { batch, .. } => batch,
            TransientPublicationBatchOutcome::Pending(obligation) => {
                panic!("zero-publication batch remained pending: {}", obligation.error())
            }
            _ => panic!("zero-publication batch did not remain retryable as no effect"),
        };
        assert_eq!(batch.len(), 2);
        for stage in batch.into_stages() {
            let destination = match stage.discard() {
                TransientDiscardOutcome::Discarded(destination) => destination,
                TransientDiscardOutcome::Pending(obligation) => {
                    panic!("zero-publication discard remained pending: {}", obligation.error())
                }
            };
            match destination.cancel() {
                TransientDestinationCancelOutcome::Cancelled => {}
                TransientDestinationCancelOutcome::Pending(obligation) => {
                    panic!("zero-publication cancellation remained pending: {}", obligation.error())
                }
            }
        }
        assert_eq!(
            std::fs::read(temporary.path().join("zero-first.bin"))
                .expect("first external collision remains"),
            b"first collision",
        );
        assert_eq!(
            std::fs::read(temporary.path().join("zero-second.bin"))
                .expect("second external collision remains"),
            b"second collision",
        );
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

        let batch = TransientPublicationBatch::new(vec![sealed])
            .expect("singleton publication batch");
        let published = match batch.publish_create_new() {
            TransientPublicationBatchOutcome::Published(mut files) => {
                files.pop().expect("singleton published file")
            }
            TransientPublicationBatchOutcome::NoEffect { error, .. } => {
                panic!("publication had no effect: {error}")
            }
            TransientPublicationBatchOutcome::Partial { error, .. } => {
                panic!("publication was partial: {error}")
            }
            TransientPublicationBatchOutcome::Pending(obligation) => {
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
        let obligation = TransientPublicationBatchObligation {
            error: io::Error::other("injected published settlement"),
            batch: Some(
                TransientPublicationBatch::new(vec![sealed])
                    .expect("singleton publication batch"),
            ),
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
            let _transition = TransientPublicationTransition::from_linked(sealed);
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
    fn publication_batch_unwind_after_partial_link_retains_root_authority() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let (sealed, id) = linked_test_stage(&root, "classification-unwind.bin");

        let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _batch = TransientPublicationBatch::new(vec![sealed])
                .expect("singleton publication batch");
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
            let operation = enter_transient_operation(sealed.stage.destination())
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
            let operation = enter_transient_operation(sealed.stage.destination())
                .expect("transient settlement operation");
            let batch = TransientPublicationBatch::new(vec![sealed])
                .expect("singleton publication batch");
            match classify_publication_batch(
                io::Error::other("injected relinked publication settlement"),
                batch,
                &operation,
            ) {
                TransientPublicationBatchOutcome::Published(mut files) => {
                    files.pop().expect("singleton published file")
                }
                TransientPublicationBatchOutcome::NoEffect { error, .. } => {
                    panic!("relinked publication had no effect: {error}")
                }
                TransientPublicationBatchOutcome::Partial { error, .. } => {
                    panic!("relinked publication was partial: {error}")
                }
                TransientPublicationBatchOutcome::Pending(obligation) => {
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
        let batch = TransientPublicationBatch::new(vec![sealed])
            .expect("singleton publication batch");
        let preserved = match batch.publish_create_new() {
            TransientPublicationBatchOutcome::NoEffect { batch, .. } => batch
                .into_stages()
                .pop()
                .expect("singleton preserved stage"),
            TransientPublicationBatchOutcome::Published(_) => {
                panic!("portable collision unexpectedly published")
            }
            TransientPublicationBatchOutcome::Partial { .. } => {
                panic!("prepublication collision unexpectedly became partial")
            }
            TransientPublicationBatchOutcome::Pending(obligation) => {
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
