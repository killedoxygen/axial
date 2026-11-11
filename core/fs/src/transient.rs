use crate::{
    AUTHORITY_DRAINING, AUTHORITY_LIVE, CapabilityAuthority, CapabilityOperation, Directory,
    EntryKind, FileCapability, LeafName, LeafNameEquivalenceKey, MAX_DIRECTORY_LIST_ENTRIES,
    MAX_OUTSTANDING_EFFECTS, leaf_name_equivalence_keys, leaf_names_equivalent, platform,
    stale_capability,
};
#[cfg(target_os = "linux")]
use crate::{AUTHORITY_QUIESCING, terminal_effect_settlement_admits};
use std::collections::HashMap;
use std::io::{self, Read, Seek, SeekFrom};
use std::mem::MaybeUninit;
use std::ops::ControlFlow;
use std::sync::Arc;

#[cfg(not(target_os = "linux"))]
mod named;
#[cfg(not(target_os = "linux"))]
use named::{
    DiscardTransientFileError, TransientFile, discard_transient_file, into_published_file,
    link_transient_file, read_transient_at, seal_transient_file, transient_file_evidence,
    write_transient_at,
};
#[cfg(target_os = "linux")]
use platform::{
    DiscardTransientFileError, TransientFile, discard_transient_file, into_published_file,
    link_transient_file, read_transient_at, seal_transient_file, transient_file_evidence,
    write_transient_at,
};

// Unicode folding and canonical decomposition can expand one admitted input
// unit into several UTF-8 scalars.
const MAX_DESTINATION_INVENTORY_SCANS: usize = 8;
#[cfg(test)]
std::thread_local! {
    static DESTINATION_INVENTORY_TEST_HOOK: std::cell::RefCell<Option<Box<dyn FnMut()>>> =
        const { std::cell::RefCell::new(None) };
}
#[cfg(target_os = "linux")]
const MAX_TRANSIENT_BATCH: usize = MAX_OUTSTANDING_EFFECTS;
// Named members retain both the destination reservation and the existing
// native stage receipt. Keep the total native effect bound unchanged.
#[cfg(not(target_os = "linux"))]
const MAX_TRANSIENT_BATCH: usize = MAX_OUTSTANDING_EFFECTS / 2;

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
    pub(super) retained: Option<TransientFile>,
    #[cfg(not(target_os = "linux"))]
    pub(super) stage_id: Option<u64>,
    pub(super) checked_out: bool,
    preserve_external: bool,
    pub(super) phase: TransientEffectPhase,
    pub(super) disposition: TransientEffectDisposition,
}

pub(super) fn stage_retained_for_drain(state: &crate::OperationState, stage_id: u64) -> bool {
    #[cfg(not(target_os = "linux"))]
    return state.transients.values().any(|record| {
        record.stage_id == Some(stage_id)
            && record.phase == TransientEffectPhase::Abandoned
            && record.retained.is_some()
    });
    #[cfg(target_os = "linux")]
    {
        let _ = (state, stage_id);
        false
    }
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

    #[cfg(all(test, target_os = "linux"))]
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
        if names.is_empty() || names.len() > MAX_TRANSIENT_BATCH {
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
                #[cfg(not(target_os = "linux"))]
                stage_id: None,
                checked_out: false,
                preserve_external: false,
                phase: TransientEffectPhase::Reserved,
                disposition: TransientEffectDisposition::Reserved,
            });
        }
        let mut tokens = Vec::new();
        tokens
            .try_reserve_exact(plan.names.len())
            .map_err(|_| io::Error::other("transient effect token capacity is exhausted"))?;
        let mut state = authority.state()?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        for name in &plan.names {
            if state.namespace_leaf_is_reserved(directory, name) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "transient destination is reserved by another filesystem effect",
                ));
            }
        }
        let first_id = state.next_id;
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
        state.next_id = next_id;
        for (offset, record) in records.into_iter().enumerate() {
            let offset =
                u64::try_from(offset).expect("bounded transient destination offset fits in u64");
            let id = first_id
                .checked_add(offset)
                .expect("prechecked transient effect id range");
            let previous = state.transients.insert(id, record);
            debug_assert!(
                previous.is_none(),
                "prechecked transient effect id is vacant"
            );
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
        let mut state = first.authority.state()?;
        if state.active == 0 {
            return Err(stale_capability());
        }
        for token in tokens.iter() {
            let record = state
                .transients
                .get(&token.id)
                .ok_or_else(stale_capability)?;
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
            debug_assert!(
                removed.is_some(),
                "prechecked transient effect is registered"
            );
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
        let published = members
            .iter()
            .filter(|member| member.is_published())
            .count();
        let outstanding_effects = state
            .outstanding_effects
            .checked_sub(published)
            .ok_or(io::ErrorKind::PermissionDenied)?;
        for member in members.iter() {
            if member.is_published() {
                let removed = state.transients.remove(&member.token().id);
                debug_assert!(
                    removed.is_some(),
                    "prechecked transient effect is registered"
                );
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

    fn mark_live(
        &self,
        identity: platform::Identity,
        #[cfg(not(target_os = "linux"))] stage_id: u64,
    ) -> io::Result<()> {
        let mut state = self.authority.state()?;
        let record = state
            .transients
            .get_mut(&self.id)
            .ok_or_else(stale_capability)?;
        if record.phase != TransientEffectPhase::Reserved {
            return Err(stale_capability());
        }
        record.phase = TransientEffectPhase::Live;
        record.identity = Some(identity);
        #[cfg(not(target_os = "linux"))]
        {
            record.stage_id = Some(stage_id);
        }
        record.disposition = TransientEffectDisposition::Staged;
        Ok(())
    }

    fn mark_disposition(&self, disposition: TransientEffectDisposition) -> io::Result<()> {
        let mut state = self.authority.state()?;
        let record = state
            .transients
            .get_mut(&self.id)
            .ok_or_else(stale_capability)?;
        if !matches!(
            record.phase,
            TransientEffectPhase::Reserved | TransientEffectPhase::Live
        ) {
            return Err(stale_capability());
        }
        record.disposition = disposition;
        Ok(())
    }

    fn reset_reserved(&self) -> io::Result<()> {
        let mut state = self.authority.state()?;
        let record = state
            .transients
            .get_mut(&self.id)
            .ok_or_else(stale_capability)?;
        if record.retained.is_some()
            || !matches!(
                record.phase,
                TransientEffectPhase::Reserved | TransientEffectPhase::Live
            )
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
        #[cfg(not(target_os = "linux"))]
        {
            record.stage_id = None;
        }
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
        retained: TransientFile,
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

impl CapabilityAuthority {
    fn settle_transient_effect(&self, id: u64, operation: &CapabilityOperation) -> io::Result<()> {
        if !std::ptr::eq(operation.authority.as_ref(), self) {
            return Err(stale_capability());
        }
        let mut state = self.state()?;
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
        // Only the root's internal abandoned-carrier settlement may re-enter
        // native receipt APIs while draining. No caller receives this scope.
        #[cfg(not(target_os = "linux"))]
        let _settlement = crate::TerminalEffectSettlementScope::begin(self, 0)?;
        let (record, operation) = {
            let mut state = self.state()?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let header = state.transients.get(&id).ok_or_else(stale_capability)?;
            if header.phase != TransientEffectPhase::Abandoned || header.checked_out {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "transient effect authority is still live",
                ));
            }
            let Some(active) = state.active.checked_add(1) else {
                return Err(io::Error::other(
                    "filesystem capability operation count overflowed",
                ));
            };
            let header = state
                .transients
                .get_mut(&id)
                .expect("prevalidated transient header remains registered");
            header.checked_out = true;
            let record = TransientEffectRecord {
                directory: header.directory.clone(),
                destination: header.destination.clone(),
                identity: header.identity,
                retained: header.retained.take(),
                #[cfg(not(target_os = "linux"))]
                stage_id: header.stage_id,
                checked_out: true,
                preserve_external: header.preserve_external,
                phase: header.phase,
                disposition: header.disposition,
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                    windowed: false,
                    remembers_files: false,
                    window_effect: crate::note_window_effect(self),
                },
            )
        };
        crate::forget_window_files_or_fail(self);
        #[cfg(not(target_os = "linux"))]
        let mut record = record;
        #[cfg(not(target_os = "linux"))]
        if let (Some(identity), Some(retained)) = (record.identity, record.retained.take()) {
            match discard_transient_file(retained, identity) {
                Ok(()) => record.disposition = TransientEffectDisposition::NoEffect,
                Err(DiscardTransientFileError::Retained { file, .. }) => {
                    record.retained = Some(file);
                }
            }
        }
        let result = match (
            record.disposition,
            record.identity,
            record.retained.as_ref(),
        ) {
            (TransientEffectDisposition::NoEffect, _, None) => Ok(()),
            (
                TransientEffectDisposition::Published | TransientEffectDisposition::Indeterminate,
                Some(identity),
                Some(retained),
            ) => validate_terminal_publication(&record, retained, identity, &operation),
            _ => Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "abandoned transient topology remains indeterminate",
            )),
        };
        if let Err(error) = result {
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let header = state
                .transients
                .get_mut(&id)
                .expect("checked-out transient header remains registered");
            assert!(header.checked_out);
            header.retained = record.retained;
            header.checked_out = false;
            return Err(error);
        }
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let removed = state
            .transients
            .remove(&id)
            .expect("settled transient header remains registered");
        assert!(removed.checked_out);
        state.release_effect(&operation);
        Ok(())
    }
}

fn validate_terminal_publication(
    record: &TransientEffectRecord,
    retained: &TransientFile,
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
        if transient_file_evidence(retained)? != (identity, 1)
            || platform::file_binding_state(
                record.directory.inner.handle(),
                record.destination.as_os_str(),
                identity,
            )? != platform::BindingState::Exact
            || !validate_portable_destination_with_operation(&destination, false, operation)?
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "abandoned transient publication is not uniquely bound to its exact destination",
            ));
        }
        Ok(())
    };
    validate()?;
    platform::sync_directory(record.directory.inner.handle())?;
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

impl_redacted_debug!(TransientDestination);

impl TransientDestination {
    pub fn name(&self) -> &LeafName {
        &self.name
    }

    pub fn directory(&self) -> &Directory {
        &self.directory
    }

    pub fn create_stage(self) -> TransientStageCreateOutcome {
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
        #[cfg(not(target_os = "linux"))]
        return named::create_stage(self);
        #[cfg(target_os = "linux")]
        let mut destination = self;
        #[cfg(target_os = "linux")]
        match platform::create_transient_file(destination.directory.inner.handle()) {
            Ok((file, identity)) => {
                let token = destination
                    .token
                    .take()
                    .expect("admitted transient destination retains its effect token")
                    .into_effect_token();
                let stage = TransientStage {
                    destination: Some(destination),
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
                    return TransientStageCreateOutcome::Pending(TransientCreationObligation {
                        error,
                        state: Some(TransientCreationState::Stage(stage)),
                    });
                }
                TransientStageCreateOutcome::Created(stage)
            }
            Err(platform::CreateTransientFileError::NoEffect(error)) => {
                TransientStageCreateOutcome::NoEffect { error, destination }
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

impl_redacted_debug!(TransientDestinationCancelOutcome);

#[must_use = "pending transient destination cancellation must be reconciled"]
pub struct TransientDestinationCancelObligation {
    error: io::Error,
    destination: Option<TransientDestination>,
}

impl_redacted_debug!(TransientDestinationCancelObligation);

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
    pub fn reconcile(mut self) -> TransientDestinationCancelOutcome {
        self.destination
            .take()
            .expect("destination cancellation obligation retains its authority")
            .cancel()
    }
}

impl_error_accessor!(TransientDestinationCancelObligation);

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
        let mut tokens = TransientEffectToken::reserve_batch(&authority, &operation, self, &plan)?;
        if let Err(error) = validate_destination_batch_with_operation(
            self,
            &plan,
            DestinationCollisionPolicy::RequireVacant,
            &operation,
        ) {
            let cleanup = TransientEffectToken::settle_no_effect_batch(&mut tokens, &operation);
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

    pub fn admit_transient_destination(&self, name: LeafName) -> io::Result<TransientDestination> {
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
    let operation = CapabilityOperation {
        window_effect: crate::note_window_effect(&authority),
        authority,
        windowed: false,
        remembers_files: false,
    };
    let directory = &batch.directory;
    let directory_buffer = batch.directory_buffer.as_mut_slice();
    platform::validate_lease_preallocated(&operation.authority.lease, &mut *directory_buffer)?;
    platform::validate_root_preallocated(&operation.authority.root, &mut *directory_buffer)?;
    crate::forget_window_files(&operation.authority)?;
    crate::confirm_observation_window(&operation.authority, true)?;
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

impl_redacted_debug!(TransientStageCreateOutcome);

enum TransientCreationState {
    Stage(TransientStage),
    #[cfg(not(target_os = "linux"))]
    Named {
        destination: TransientDestination,
        obligation: crate::FileCreateObligation,
    },
}

#[must_use = "pending transient creation authority must be reconciled"]
pub struct TransientCreationObligation {
    error: io::Error,
    state: Option<TransientCreationState>,
}

impl_redacted_debug!(TransientCreationObligation);

impl TransientCreationObligation {
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
                    .mark_live(
                        stage.identity,
                        #[cfg(not(target_os = "linux"))]
                        stage.file.as_ref().expect("stage retains file").stage_id,
                    );
                match result {
                    Ok(()) => TransientStageCreateOutcome::Created(stage),
                    Err(error) => TransientStageCreateOutcome::Pending(Self {
                        error,
                        state: Some(TransientCreationState::Stage(stage)),
                    }),
                }
            }
            #[cfg(not(target_os = "linux"))]
            TransientCreationState::Named {
                destination,
                obligation,
            } => named::reconcile_creation(destination, obligation),
        }
    }
}

impl_error_accessor!(TransientCreationObligation);

#[must_use = "a transient stage must be sealed or explicitly discarded"]
pub struct TransientStage {
    destination: Option<TransientDestination>,
    file: Option<TransientFile>,
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
            let written = write_transient_at(file, bytes, self.position)?;
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

    /// Runs `work` in one batch validation window of the stage directory, so
    /// its writes share one ancestry validation. A displacement fails when the
    /// window closes; sealing validates fully afterwards. Linux stages are
    /// unnamed and need no window.
    pub fn batch_validated<T>(&mut self, work: impl FnOnce(&mut Self) -> T) -> io::Result<T> {
        #[cfg(target_os = "linux")]
        return Ok(work(self));
        #[cfg(not(target_os = "linux"))]
        {
            let directory = self.destination().directory.clone();
            directory.batch_validated(|| work(self))
        }
    }

    pub fn seal(mut self) -> Result<TransientStageSealed, TransientStageSealFailure> {
        let file = self
            .file
            .as_mut()
            .expect("live transient stage retains its file");
        if let Err(error) = seal_transient_file(file, self.identity, self.position) {
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
        let file = self
            .file
            .take()
            .expect("live transient stage retains its file");
        match discard_transient_file(file, self.identity) {
            Ok(()) => {
                let token = self
                    .token
                    .take()
                    .expect("live transient stage retains its effect token");
                let destination = self.take_destination();
                restore_discarded_destination(destination, token)
            }
            Err(DiscardTransientFileError::Retained { error, file }) => {
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
            destination.directory.inner.handle(),
            destination.name.as_os_str(),
            self.identity,
        );
        #[cfg(target_os = "linux")]
        let discard = platform::discard_transient_file_preallocated(file, self.identity);
        #[cfg(not(target_os = "linux"))]
        let discard = discard_transient_file(file, self.identity);
        match discard {
            Ok(()) => {
                if let Some(token) = self.token.as_ref() {
                    token.mark_disposition_on_drop(TransientEffectDisposition::NoEffect);
                }
            }
            Err(DiscardTransientFileError::Retained { file, .. }) => {
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

impl_redacted_debug!(TransientStageSealFailure);

impl TransientStageSealFailure {
    pub fn into_stage(mut self) -> TransientStage {
        self.stage.take().expect("seal failure retains its stage")
    }
}

impl_error_accessor!(TransientStageSealFailure);

#[must_use = "a sealed transient stage must be published or explicitly discarded"]
pub struct TransientStageSealed {
    stage: TransientStage,
    read_position: u64,
}

impl_redacted_debug!(TransientStageSealed);

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
        let read = read_transient_at(file, &mut bytes[..allowed], self.read_position)?;
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
    retained: &TransientFile,
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
    retained: &TransientFile,
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
            destination.directory.inner.handle(),
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
    retained: &TransientFile,
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
        destination.directory.inner.handle(),
        destination.name.as_os_str(),
        identity,
    )?;
    #[cfg(target_os = "linux")]
    let unpublished = transient_file_evidence_for_publication(retained)? == (identity, 0);
    #[cfg(not(target_os = "linux"))]
    let unpublished = named::validate_unpublished(retained, identity).is_ok();
    if !unpublished || binding == platform::BindingState::Exact {
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
            let identity = match &current.storage {
                crate::DirectoryStorage::Owned(handle) => {
                    platform::directory_identity_preallocated(handle)?
                }
                crate::DirectoryStorage::Absolute(guard) => {
                    platform::validate_absolute_directory_guard_preallocated(guard, buffer)?
                }
            };
            if identity != current.identity.physical {
                return Err(io::ErrorKind::InvalidData.into());
            }
            let Some(binding) = &current.parent else {
                break;
            };
            if platform::directory_binding_state(
                binding.directory.inner.handle(),
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
        return platform::directory_revision_preallocated(directory.inner.handle());
    }
    #[cfg(not(target_os = "linux"))]
    debug_assert!(directory_buffer.is_none());
    platform::directory_revision(directory.inner.handle())
}

fn transient_publication_state_for_publication(
    transient: &TransientFile,
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
    named::transient_publication_state(transient, parent, destination_name, expected)
}

fn transient_file_evidence_for_publication(
    transient: &TransientFile,
) -> io::Result<(platform::Identity, u64)> {
    #[cfg(target_os = "linux")]
    return platform::transient_file_evidence_preallocated(transient);
    #[cfg(not(target_os = "linux"))]
    transient_file_evidence(transient)
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
        .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("portable leaf proof capacity is exhausted"))?;
    native_key
        .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("native leaf proof capacity is exhausted"))?;
    normalization
        .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
        .map_err(|_| io::Error::other("leaf normalization capacity is exhausted"))?;
    let mut inventory = DestinationBatchInventory {
        expected_exact: &expected_exact,
        exact: &mut exact,
        portable_key: &mut portable_key,
        native_key: &mut native_key,
        normalization: &mut normalization,
        collision_policy,
        directory_buffer: None,
    };
    validate_mixed_destination_batch_with_operation(directory, plan, &mut inventory, operation)?;
    Ok(exact)
}

struct DestinationBatchInventory<'a> {
    expected_exact: &'a [bool],
    exact: &'a mut [bool],
    portable_key: &'a mut Vec<u8>,
    native_key: &'a mut Vec<u8>,
    normalization: &'a mut Vec<(u8, char)>,
    collision_policy: DestinationCollisionPolicy,
    directory_buffer: Option<&'a mut [MaybeUninit<u8>]>,
}

fn validate_mixed_destination_batch_with_operation(
    directory: &Directory,
    plan: &DestinationBatchPlan,
    inventory: &mut DestinationBatchInventory<'_>,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    // Sibling scratch creation/removal can invalidate a read-only inventory.
    // Keep the same reservations and operation; never retry a namespace effect.
    for _ in 0..MAX_DESTINATION_INVENTORY_SCANS {
        if observe_destination_inventory(directory, plan, inventory, operation)? {
            return Ok(());
        }
    }
    Err(io::ErrorKind::WouldBlock.into())
}

/// Returns false when the directory changed during a complete scan.
fn observe_destination_inventory(
    directory: &Directory,
    plan: &DestinationBatchPlan,
    inventory: &mut DestinationBatchInventory<'_>,
    operation: &CapabilityOperation,
) -> io::Result<bool> {
    let DestinationBatchInventory {
        expected_exact,
        exact,
        portable_key,
        native_key,
        normalization,
        collision_policy,
        directory_buffer,
    } = inventory;
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
        if let (Some(portable_target), Some(native_target)) = (portable_target, native_target)
            && portable_target != native_target
        {
            if *collision_policy == DestinationCollisionPolicy::AllowExternalCollision
                && !expected_exact[portable_target]
                && !expected_exact[native_target]
            {
                return Ok(ControlFlow::Continue(()));
            }
            conflict = Some(io::ErrorKind::AlreadyExists.into());
            return Ok(ControlFlow::Break(()));
        }
        let target = portable_target.or(native_target);
        let Some(target) = target else {
            return Ok(ControlFlow::Continue(()));
        };
        if *collision_policy == DestinationCollisionPolicy::RequireVacant {
            conflict = Some(io::ErrorKind::AlreadyExists.into());
            return Ok(ControlFlow::Break(()));
        }
        if !expected_exact[target] {
            return Ok(ControlFlow::Continue(()));
        }
        let target_name = plan.names[target].as_os_str();
        let error = if observed_name != target_name || exact[target] || kind != EntryKind::File {
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
            directory.inner.handle(),
            buffer,
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        ),
        None => platform::visit_entries(
            directory.inner.handle(),
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        ),
    };
    #[cfg(not(target_os = "linux"))]
    let visit = {
        debug_assert!(directory_buffer.is_none());
        platform::visit_entries(
            directory.inner.handle(),
            MAX_DIRECTORY_LIST_ENTRIES,
            &mut visit_entry,
        )
    };
    #[cfg(test)]
    DESTINATION_INVENTORY_TEST_HOOK.with(|hook| {
        if let Some(hook) = hook.borrow_mut().as_mut() {
            hook();
        }
    });
    validate_publication_directory(directory, operation, directory_buffer.as_deref_mut())?;
    let revision_after =
        directory_revision_for_publication(directory, directory_buffer.as_deref_mut())?;
    let completion = visit?;
    // Observed collisions and native failures are terminal even during churn.
    // Equal stamps never replace a complete inventory.
    match completion {
        platform::VisitCompletion::Complete => Ok(revision_after == revision_before),
        platform::VisitCompletion::Stopped => {
            Err(conflict
                .expect("transient destination inventory stops only for a decisive conflict"))
        }
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

impl_redacted_debug!(TransientPublicationBatchCreateFailure);

impl TransientPublicationBatchCreateFailure {
    pub fn into_stages(mut self) -> Vec<TransientStageSealed> {
        self.stages
            .take()
            .expect("publication batch creation failure retains every stage")
    }
}

impl_error_accessor!(TransientPublicationBatchCreateFailure);

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

impl_redacted_debug!(TransientPublicationBatchOutcome);

struct TransientPublicationTransition {
    stage: Option<TransientStageSealed>,
    identity: platform::Identity,
    retained: Option<TransientFile>,
    token: Option<TransientEffectToken>,
}

impl TransientPublicationTransition {
    fn from_linked(stage: TransientStageSealed) -> Self {
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
        self.stage
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
            into_published_file(retained),
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
            let _retained = self.retained.take();
            if let Some(stage) = self.stage.as_mut() {
                let _stage_file = stage.stage.file.take();
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

impl_redacted_debug!(TransientPublicationBatchObligation);

impl TransientPublicationBatch {
    pub fn new(
        stages: Vec<TransientStageSealed>,
    ) -> Result<Self, TransientPublicationBatchCreateFailure> {
        if stages.is_empty() || stages.len() > MAX_TRANSIENT_BATCH {
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
                .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
                .is_err()
            || native_key
                .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
                .is_err()
            || normalization
                .try_reserve_exact(platform::MAX_EQUIVALENCE_KEY_BYTES)
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
            let link = link_transient_file(
                file,
                destination.directory.inner.handle(),
                destination.name.as_os_str(),
            );
            let state = transient_publication_state_for_publication(
                file,
                destination.directory.inner.handle(),
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
                    return pending_publication(io::ErrorKind::InvalidData.into(), self);
                }
            }
        }
        classify_publication_batch(io::ErrorKind::Other.into(), self, &operation)
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
            destination.directory.inner.handle(),
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
                return pending_publication(io::ErrorKind::WouldBlock.into(), batch);
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
    if let Err(sync) = platform::sync_directory(batch.directory.inner.handle()) {
        return pending_publication(sync, batch);
    }
    #[cfg(target_os = "linux")]
    let directory_buffer = Some(batch.directory_buffer.as_mut_slice());
    #[cfg(not(target_os = "linux"))]
    let directory_buffer = None;
    let mut inventory = DestinationBatchInventory {
        expected_exact: &batch.classifications,
        exact: &mut batch.inventory_exact,
        portable_key: &mut batch.portable_key,
        native_key: &mut batch.native_key,
        normalization: &mut batch.normalization,
        collision_policy: DestinationCollisionPolicy::AllowExternalCollision,
        directory_buffer,
    };
    if let Err(proof) = validate_mixed_destination_batch_with_operation(
        &batch.directory,
        &batch.plan,
        &mut inventory,
        operation,
    ) {
        return pending_publication(proof, batch);
    }
    if batch.inventory_exact != batch.classifications {
        return pending_publication(io::ErrorKind::InvalidData.into(), batch);
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
    for (stage, published) in stages.drain(..).zip(classifications.iter().copied()) {
        if published {
            classified.push(ClassifiedPublicationMember::Published(
                TransientPublicationTransition::from_linked(stage),
            ));
        } else {
            classified.push(ClassifiedPublicationMember::Unpublished(stage));
        }
    }
    if let Err(settlement) =
        TransientEffectToken::settle_classified_batch(&mut classified, operation)
    {
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
    pub fn reconcile(mut self) -> TransientPublicationBatchOutcome {
        let error = std::mem::replace(&mut self.error, io::ErrorKind::Other.into());
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

impl_error_accessor!(TransientPublicationBatchObligation);

#[must_use = "transient discard outcomes must retain failed cleanup authority"]
pub enum TransientDiscardOutcome {
    Discarded(TransientDestination),
    Pending(TransientDiscardObligation),
}

impl_redacted_debug!(TransientDiscardOutcome);

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

impl_redacted_debug!(TransientDiscardObligation);

impl TransientDiscardObligation {
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

impl_error_accessor!(TransientDiscardObligation);

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

impl crate::AdmittedAbsoluteDirectory {
    /// Admit one create-only external artifact with preserve-only settlement.
    /// The permission is captured while the complete ancestry is still exact.
    pub fn admit_transient_destination(&self, name: LeafName) -> io::Result<TransientDestination> {
        let directory = &self.inner.directory;
        let authority = directory.authority()?;
        let operation = authority.enter()?;
        directory.validate(&operation)?;
        let ancestry = directory
            .inner
            .absolute_ancestry()
            .ok_or_else(|| io::Error::other("external destination has no captured ancestry"))?;
        if !platform::absolute_directory_is_outside_root(ancestry, &authority.root)? {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "external destination is inside the owning root",
            ));
        }
        let destination = directory.admit_transient_destination(name)?;
        let token = destination
            .token
            .as_ref()
            .expect("admitted destination retains reservation")
            .token
            .as_ref()
            .expect("reservation retains effect token");
        let mut state = authority.state()?;
        state
            .transients
            .get_mut(&token.id)
            .ok_or_else(stale_capability)?
            .preserve_external = true;
        drop(state);
        Ok(destination)
    }
}

impl TransientEffectToken {
    fn preservation_operation(&self) -> io::Result<CapabilityOperation> {
        let operation = self.authority.enter()?;
        let state = self.authority.state()?;
        let record = state
            .transients
            .get(&self.id)
            .ok_or_else(stale_capability)?;
        if !self.armed
            || !record.preserve_external
            || record.checked_out
            || record.retained.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "transient effect has no external preservation admission",
            ));
        }
        drop(state);
        Ok(operation)
    }

    fn acknowledge_preserved(&mut self) -> io::Result<()> {
        let operation = self.preservation_operation()?;
        self.finish_preservation(&operation)
    }

    fn finish_preservation(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        let mut state = self.authority.state()?;
        let record = state
            .transients
            .get(&self.id)
            .ok_or_else(stale_capability)?;
        if !self.armed
            || !record.preserve_external
            || record.checked_out
            || record.retained.is_some()
        {
            return Err(stale_capability());
        }
        let removed = state
            .transients
            .remove(&self.id)
            .expect("validated preservation owner");
        state.release_effect(operation);
        self.armed = false;
        drop(state);
        drop(removed);
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn preserve_named_stage(&mut self, token: &mut crate::StageToken) -> io::Result<()> {
        let operation = self.preservation_operation()?;
        let mut state = self.authority.state()?;
        let header = state
            .transients
            .get(&self.id)
            .ok_or_else(stale_capability)?;
        if token.armed {
            if token.authority.as_ptr() != Arc::as_ptr(&self.authority)
                || header.stage_id != Some(token.id)
            {
                return Err(stale_capability());
            }
            let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
            if stage.cleanup.is_none()
                || stage.recovery.is_some()
                || stage
                    .promotion
                    .as_ref()
                    .is_some_and(|promotion| promotion.displaced_park.is_some())
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "recoverable or replaced state cannot be preserved as an export",
                ));
            }
        }
        let removed_stage = token.armed.then(|| {
            state
                .stages
                .remove(&token.id)
                .expect("validated named stage owner")
        });
        if removed_stage.is_some() {
            state.release_effect(&operation);
        }
        token.armed = false;
        let removed = state
            .transients
            .remove(&self.id)
            .expect("validated preservation owner");
        state.release_effect(&operation);
        self.armed = false;
        drop(state);
        // Cleanup handles are closed; no unlink, restore, or replacement occurs.
        drop((removed_stage, removed));
        Ok(())
    }

    #[cfg(not(target_os = "linux"))]
    fn preserve_named_creation(&mut self, token: &mut crate::StageCreateToken) -> io::Result<()> {
        let operation = self.preservation_operation()?;
        let mut state = self.authority.state()?;
        let header = state
            .transients
            .get(&self.id)
            .ok_or_else(stale_capability)?;
        if !token.armed || token.authority.as_ptr() != Arc::as_ptr(&self.authority) {
            return Err(stale_capability());
        }
        let create = state
            .stage_creations
            .get(&token.id)
            .ok_or_else(stale_capability)?;
        if create.checked_out
            || create.parent.inner.identity != header.directory.inner.identity
            || create.recovery.is_some()
            || create.recovery_intent.is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "creation has no fresh external export ownership",
            ));
        }
        let removed_create = state
            .stage_creations
            .remove(&token.id)
            .expect("validated named creation owner");
        let removed = state
            .transients
            .remove(&self.id)
            .expect("validated preservation owner");
        state.release_effect(&operation);
        state.release_effect(&operation);
        token.armed = false;
        self.armed = false;
        drop(state);
        drop((removed_create, removed));
        Ok(())
    }
}

impl TransientStage {
    fn preserve_external(&mut self) -> io::Result<()> {
        let token = self.token.as_mut().expect("stage retains its effect token");
        #[cfg(target_os = "linux")]
        token.acknowledge_preserved()?;
        #[cfg(not(target_os = "linux"))]
        named::acknowledge_preserved(self.file.as_mut().expect("stage retains its file"), token)?;
        // In particular, do not run TransientStage::Drop's discard path.
        drop(self.file.take());
        Ok(())
    }
}

impl TransientCreationObligation {
    /// Acknowledge an external fresh artifact as preserved, without asserting
    /// publication or mutating any captured namespace binding.
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let result = match self.state.as_mut().expect("creation retains its state") {
            TransientCreationState::Stage(stage) => stage.preserve_external(),
            #[cfg(not(target_os = "linux"))]
            TransientCreationState::Named {
                destination,
                obligation,
            } => destination
                .token
                .as_mut()
                .expect("creation retains reservation")
                .token_mut()
                .preserve_named_creation(&mut obligation.token),
        };
        match result {
            Ok(()) => {
                self.state.take();
                Ok(())
            }
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

impl TransientDestinationCancelObligation {
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let destination = self
            .destination
            .as_mut()
            .expect("cancellation retains its destination");
        match destination
            .token
            .as_mut()
            .expect("cancellation retains reservation")
            .token_mut()
            .acknowledge_preserved()
        {
            Ok(()) => {
                self.destination.take();
                Ok(())
            }
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

impl TransientDiscardObligation {
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let result = match self.state.as_mut().expect("discard retains its state") {
            TransientDiscardState::Stage(stage) => stage.preserve_external(),
            TransientDiscardState::ReservationRestore { token, .. } => {
                token.acknowledge_preserved()
            }
        };
        match result {
            Ok(()) => {
                self.state.take();
                Ok(())
            }
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

impl TransientPublicationBatchObligation {
    /// The native Save owner publishes exactly one fresh external artifact.
    /// Preservation never acknowledges a Published result or deletes a name.
    pub fn acknowledge_preserved_singleton(mut self) -> Result<(), Self> {
        let batch = self.batch.as_mut().expect("publication retains its batch");
        if batch.stages.len() != 1 {
            self.error = io::Error::new(
                io::ErrorKind::InvalidInput,
                "external preservation requires a singleton publication",
            );
            return Err(self);
        }
        match batch.stages[0].stage.preserve_external() {
            Ok(()) => {
                self.batch.take();
                Ok(())
            }
            Err(error) => {
                self.error = error;
                Err(self)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        MoveEffectToken, NamespaceLeaf, RootRevokeOutcome, RootSession, RootSessionAcquireOutcome,
    };
    #[cfg(target_os = "linux")]
    use std::ffi::OsStr;
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

    fn fixture() -> (tempfile::TempDir, RootSession, Directory) {
        let temporary = crate::test_tempdir().unwrap();
        let session = acquire_test_root(temporary.path());
        let root = session.root().unwrap();
        (temporary, session, root)
    }

    fn revoke(session: RootSession, root: Directory) {
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    /// Live transient records and outstanding effects.
    fn effects(session: &RootSession) -> (usize, usize) {
        let state = session.authority.operations.lock().unwrap();
        (state.transients.len(), state.outstanding_effects)
    }

    fn leaves(names: &[&str]) -> Vec<LeafName> {
        names
            .iter()
            .map(|name| LeafName::new(name).unwrap())
            .collect()
    }

    fn test_stage(root: &Directory, name: &str) -> Option<TransientStage> {
        let destination = root
            .admit_transient_destination(LeafName::new(name).unwrap())
            .unwrap();
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

    fn cancel(destination: TransientDestination) {
        assert!(matches!(
            destination.cancel(),
            TransientDestinationCancelOutcome::Cancelled
        ));
    }

    fn discard_and_cancel(outcome: TransientDiscardOutcome) {
        let TransientDiscardOutcome::Discarded(destination) = outcome else {
            panic!("transient discard remained pending")
        };
        cancel(destination);
    }

    #[cfg(target_os = "linux")]
    fn published(outcome: TransientPublicationBatchOutcome) -> Vec<FileCapability> {
        match outcome {
            TransientPublicationBatchOutcome::Published(files) => files,
            other => panic!("publication was not complete: {other:?}"),
        }
    }

    fn namespace_leaf(parent: &Directory, name: &str) -> NamespaceLeaf {
        NamespaceLeaf {
            parent: parent.clone(),
            name: LeafName::new(name).unwrap(),
        }
    }

    fn reserve_test_transient(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        directory: &Directory,
        name: &str,
    ) -> io::Result<TransientEffectToken> {
        let plan = DestinationBatchPlan::new(leaves(&[name]))?;
        let mut tokens =
            TransientEffectToken::reserve_batch(authority, operation, directory, &plan)?;
        Ok(tokens.pop().expect("singleton transient reservation"))
    }

    fn settle_transient(mut token: TransientEffectToken, operation: &CapabilityOperation) {
        token
            .mark_disposition(TransientEffectDisposition::NoEffect)
            .unwrap();
        token.settle_with(operation).unwrap();
    }

    fn with_inventory_hook<T>(hook: impl FnMut() + 'static, action: impl FnOnce() -> T) -> T {
        struct ClearHook;
        impl Drop for ClearHook {
            fn drop(&mut self) {
                DESTINATION_INVENTORY_TEST_HOOK.with(|hook| hook.borrow_mut().take());
            }
        }
        DESTINATION_INVENTORY_TEST_HOOK.with(|slot| {
            assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
        });
        let _clear = ClearHook;
        action()
    }

    fn create_and_remove_sibling(root: &std::path::Path) {
        let path = root.join("inventory-sibling-scratch");
        drop(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap(),
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn destination_inventory_rescans_after_sibling_stage_cleanup() {
        let (temporary, session, root) = fixture();
        let mut sibling = Some(temporary.path().to_path_buf());
        let batch = with_inventory_hook(
            move || {
                if let Some(sibling) = sibling.take() {
                    create_and_remove_sibling(&sibling);
                }
            },
            || root.admit_transient_destinations(leaves(&["first.bin", "second.bin"])),
        )
        .expect("unrelated scratch cleanup must not fail destination admission");
        batch.into_destinations().into_iter().for_each(cancel);
        revoke(session, root);
    }

    #[test]
    fn destination_inventory_bounds_churn_and_releases_failed_reservations() {
        let (temporary, session, root) = fixture();
        let sibling = temporary.path().to_path_buf();
        let names = leaves(&["first.bin", "second.bin"]);
        let error = with_inventory_hook(
            move || create_and_remove_sibling(&sibling),
            || root.admit_transient_destinations(names.clone()),
        )
        .expect_err("continuous namespace churn must remain bounded");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let batch = root
            .admit_transient_destinations(names)
            .expect("failed inventory must settle every destination reservation");
        batch.into_destinations().into_iter().for_each(cancel);
        revoke(session, root);
    }

    #[test]
    fn destination_inventory_rescan_preserves_portable_collision_during_churn() {
        let (temporary, session, root) = fixture();
        let alias = temporary.path().join("ARTIFACT.BIN");
        let mut introduce_alias = Some(alias.clone());
        let sibling = temporary.path().to_path_buf();
        let error = with_inventory_hook(
            move || {
                if let Some(alias) = introduce_alias.take() {
                    std::fs::write(alias, b"external contents").unwrap();
                }
                create_and_remove_sibling(&sibling);
            },
            || root.admit_transient_destination(LeafName::new("artifact.bin").unwrap()),
        )
        .expect_err("a newly observed portable alias must not be hidden by churn");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(alias).unwrap(), b"external contents");
        cancel(
            root.admit_transient_destination(LeafName::new("other.bin").unwrap())
                .unwrap(),
        );
        revoke(session, root);
    }

    #[test]
    fn batch_collisions_are_rejected_before_effect_reservation() {
        for (occupant, names) in [
            (None, ["Artifact.bin", "artifact.BIN"]),
            (Some("Occupied.bin"), ["occupied.BIN", "vacant.bin"]),
        ] {
            let (temporary, session, root) = fixture();
            if let Some(occupant) = occupant {
                std::fs::write(temporary.path().join(occupant), b"occupied").unwrap();
            }
            let error = root
                .admit_transient_destinations(leaves(&names))
                .expect_err("portable collision must reject the batch");
            assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
            assert_eq!(effects(&session), (0, 0));
            revoke(session, root);
        }
    }

    #[test]
    fn held_destination_blocks_batch_until_explicit_cancellation() {
        let (_temporary, session, root) = fixture();
        let held = root
            .admit_transient_destination(LeafName::new("Held.bin").unwrap())
            .unwrap();
        let error = root
            .admit_transient_destinations(leaves(&["held.BIN"]))
            .expect_err("held destination must block a competing batch");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        cancel(held);
        assert_eq!(effects(&session), (0, 0));
        let retried = root
            .admit_transient_destinations(leaves(&["held.BIN"]))
            .expect("destination admission after cancellation");
        retried.into_destinations().into_iter().for_each(cancel);
        revoke(session, root);
    }

    #[test]
    fn batch_admission_reserves_every_destination_and_drop_abandons_without_effect() {
        let (_temporary, session, root) = fixture();
        let batch = root
            .admit_transient_destinations(leaves(&["first.bin", "second.bin"]))
            .unwrap();
        assert_eq!(batch.len(), 2);
        {
            let state = session.authority.operations.lock().unwrap();
            assert_eq!((state.transients.len(), state.outstanding_effects), (2, 2));
            assert!(state.transients.values().all(|record| {
                record.phase == TransientEffectPhase::Reserved
                    && record.disposition == TransientEffectDisposition::Reserved
            }));
        }
        drop(batch);
        {
            let state = session.authority.operations.lock().unwrap();
            assert!(state.transients.values().all(|record| {
                record.phase == TransientEffectPhase::Abandoned
                    && record.disposition == TransientEffectDisposition::NoEffect
            }));
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discarded_stage_reuses_the_exact_destination_reservation() {
        let (_temporary, session, root) = fixture();
        let first = test_stage(&root, "retry.bin").expect("transient platform");
        let reservation = first.token.as_ref().unwrap().id;
        let TransientDiscardOutcome::Discarded(destination) = first.discard() else {
            panic!("first discard remained pending")
        };
        assert_eq!(destination.token.as_ref().unwrap().token().id, reservation);
        let error = root
            .admit_transient_destinations(leaves(&["RETRY.BIN"]))
            .expect_err("discarded destination must retain its reservation");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        let TransientStageCreateOutcome::Created(second) = destination.create_stage() else {
            panic!("second stage creation failed")
        };
        assert_eq!(second.token.as_ref().unwrap().id, reservation);
        discard_and_cancel(second.discard());
        revoke(session, root);
    }

    #[test]
    fn reserved_token_unwind_is_root_cleanable_no_effect() {
        let (_temporary, session, root) = fixture();
        let authority = session.authority.clone();
        let operation = authority.enter().unwrap();
        drop(reserve_test_transient(&authority, &operation, &root, "unwound.bin").unwrap());
        drop(operation);
        {
            let state = authority.operations.lock().unwrap();
            let record = state.transients.values().next().unwrap();
            assert!(record.phase == TransientEffectPhase::Abandoned);
            assert!(record.disposition == TransientEffectDisposition::NoEffect);
        }
        revoke(session, root);
    }

    #[test]
    fn move_and_transient_reservations_reject_conflicts_in_either_order() {
        let (_temporary, session, root) = fixture();
        let authority = session.authority.clone();
        let operation = authority.enter().unwrap();
        let mut movement = MoveEffectToken::reserve(
            &authority,
            &operation,
            namespace_leaf(&root, "source.bin"),
            namespace_leaf(&root, "Destination.bin"),
            None,
            None,
        )
        .unwrap();
        let conflict = reserve_test_transient(&authority, &operation, &root, "destination.BIN");
        assert!(matches!(conflict, Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        movement.settle(&operation).unwrap();

        let transient =
            reserve_test_transient(&authority, &operation, &root, "Source.bin").unwrap();
        let conflict = MoveEffectToken::reserve(
            &authority,
            &operation,
            namespace_leaf(&root, "source.BIN"),
            namespace_leaf(&root, "other.bin"),
            None,
            None,
        );
        assert!(matches!(conflict, Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        settle_transient(transient, &operation);
        drop(operation);
        revoke(session, root);
    }

    #[test]
    fn unrelated_sibling_tree_reservations_proceed_together() {
        let (temporary, session, root) = fixture();
        for tree in ["move-tree", "transient-tree"] {
            std::fs::create_dir(temporary.path().join(tree)).unwrap();
        }
        let move_tree = root
            .open_directory(&LeafName::new("move-tree").unwrap())
            .unwrap();
        let transient_tree = root
            .open_directory(&LeafName::new("transient-tree").unwrap())
            .unwrap();
        let authority = session.authority.clone();
        let operation = authority.enter().unwrap();
        let mut movement = MoveEffectToken::reserve(
            &authority,
            &operation,
            namespace_leaf(&move_tree, "source.bin"),
            namespace_leaf(&move_tree, "destination.bin"),
            None,
            None,
        )
        .unwrap();
        let transient =
            reserve_test_transient(&authority, &operation, &transient_tree, "destination.bin")
                .expect("unrelated transient reservation");
        settle_transient(transient, &operation);
        movement.settle(&operation).unwrap();
        drop((operation, transient_tree, move_tree));
        revoke(session, root);
    }

    #[test]
    fn simultaneous_move_and_transient_reservations_admit_exactly_one() {
        let (_temporary, session, root) = fixture();
        let authority = session.authority.clone();
        let barriers = Arc::new((Barrier::new(2), Barrier::new(2)));
        let race = |transient: bool| {
            let (authority, root, barriers) =
                (Arc::clone(&authority), root.clone(), Arc::clone(&barriers));
            thread::spawn(move || {
                let operation = authority.enter().unwrap();
                barriers.0.wait();
                let reservation = if transient {
                    reserve_test_transient(&authority, &operation, &root, "race.BIN").map(Ok)
                } else {
                    MoveEffectToken::reserve(
                        &authority,
                        &operation,
                        namespace_leaf(&root, "source.bin"),
                        namespace_leaf(&root, "Race.bin"),
                        None,
                        None,
                    )
                    .map(Err)
                };
                barriers.1.wait();
                match reservation {
                    Ok(Ok(token)) => settle_transient(token, &operation),
                    Ok(Err(mut token)) => token.settle(&operation).unwrap(),
                    Err(error) => {
                        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                        return false;
                    }
                }
                true
            })
        };
        let (moving, transient) = (race(false), race(true));
        assert_ne!(moving.join().unwrap(), transient.join().unwrap());
        {
            let state = authority.operations.lock().unwrap();
            assert!(state.moves.is_empty());
            assert!(state.transients.is_empty());
            assert_eq!(state.outstanding_effects, 0);
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    fn token_id(sealed: &TransientStageSealed) -> u64 {
        sealed.stage.token.as_ref().unwrap().id
    }

    #[cfg(target_os = "linux")]
    fn link(sealed: &mut TransientStageSealed, root: &Directory, name: &str) -> io::Result<()> {
        platform::link_transient_file(
            sealed.stage.file.as_mut().unwrap(),
            root.inner.handle(),
            OsStr::new(name),
        )
    }

    #[cfg(target_os = "linux")]
    fn linked_test_stage(root: &Directory, name: &str) -> (TransientStageSealed, u64) {
        let mut sealed = test_stage(root, name).unwrap().seal().unwrap();
        link(&mut sealed, root, name).unwrap();
        let id = token_id(&sealed);
        (sealed, id)
    }

    #[cfg(target_os = "linux")]
    fn sealed_batch(root: &Directory, names: &[&str]) -> TransientPublicationBatch {
        TransientPublicationBatch::new(
            names
                .iter()
                .map(|name| test_stage(root, name).unwrap().seal().unwrap())
                .collect(),
        )
        .unwrap()
    }

    #[cfg(target_os = "linux")]
    fn classify_collision(batch: TransientPublicationBatch) -> TransientPublicationBatchOutcome {
        let operation = enter_transient_operation(batch.stages[0].stage.destination()).unwrap();
        classify_publication_batch(
            io::Error::new(io::ErrorKind::AlreadyExists, "injected collision"),
            batch,
            &operation,
        )
    }

    #[cfg(target_os = "linux")]
    fn assert_retained_transient(
        session: &RootSession,
        id: u64,
        disposition: TransientEffectDisposition,
    ) {
        let state = session.authority.operations.lock().unwrap();
        let record = state
            .transients
            .get(&id)
            .expect("retained transient record");
        assert!(record.phase == TransientEffectPhase::Abandoned);
        assert!(record.disposition == disposition);
        assert!(record.retained.is_some());
    }

    #[test]
    fn dropped_pending_carriers_remain_root_owned_until_terminal_cleanup() {
        let (_temporary, session, root) = fixture();
        let Some(create_stage) = test_stage(&root, "create-pending.bin") else {
            return revoke(session, root);
        };
        drop(TransientCreationObligation {
            error: io::Error::other("injected creation settlement"),
            state: Some(TransientCreationState::Stage(create_stage)),
        });
        let publication_stage = test_stage(&root, "publication-pending.bin").unwrap();
        drop(TransientPublicationBatchObligation {
            error: io::Error::other("injected publication settlement"),
            batch: Some(
                TransientPublicationBatch::new(vec![publication_stage.seal().unwrap()]).unwrap(),
            ),
        });
        let discard_stage = test_stage(&root, "discard-pending.bin").unwrap();
        drop(TransientDiscardObligation {
            error: io::Error::other("injected discard settlement"),
            state: Some(TransientDiscardState::Stage(discard_stage)),
        });
        {
            let state = session.authority.operations.lock().unwrap();
            assert_eq!((state.transients.len(), state.outstanding_effects), (3, 3));
            assert!(
                state
                    .transients
                    .values()
                    .all(|record| record.phase == TransientEffectPhase::Abandoned)
            );
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_publication_binds_exact_single_link_content() {
        let (temporary, session, root) = fixture();
        let mut first = test_stage(&root, "group-first.bin").unwrap();
        let mut second = test_stage(&root, "group-second.bin").unwrap();
        first.write_all(b"first").unwrap();
        second.write_all(b"second").unwrap();
        let batch =
            TransientPublicationBatch::new(vec![first.seal().unwrap(), second.seal().unwrap()])
                .unwrap();
        let files = published(batch.publish_create_new());
        assert_eq!(files.len(), 2);
        for (file, (name, bytes)) in files.iter().zip([
            ("group-first.bin", b"first".as_slice()),
            ("group-second.bin", b"second"),
        ]) {
            assert_eq!(std::fs::read(temporary.path().join(name)).unwrap(), bytes);
            assert_eq!(
                platform::exact_file_link_count(
                    root.inner.handle(),
                    OsStr::new(name),
                    file.identity
                )
                .unwrap(),
                Some(1),
            );
        }
        assert_eq!(effects(&session), (0, 0));
        drop(files);
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_partial_publication_keeps_member_order_for_each_stable_collision() {
        for (target, collision, directory) in [
            ("exact-target.bin", "exact-target.bin", false),
            ("portable-target.bin", "PORTABLE-TARGET.BIN", false),
            ("wrong-kind-target.bin", "wrong-kind-target.bin", true),
        ] {
            let (temporary, session, root) = fixture();
            let mut batch = sealed_batch(&root, &["published.bin", target]);
            let (first_id, second_id) = (token_id(&batch.stages[0]), token_id(&batch.stages[1]));
            link(&mut batch.stages[0], &root, "published.bin").unwrap();
            let collision = temporary.path().join(collision);
            if directory {
                std::fs::create_dir(&collision).unwrap();
            } else {
                std::fs::write(&collision, b"external collision").unwrap();
            }
            let TransientPublicationBatchOutcome::Partial { members, .. } =
                classify_collision(batch)
            else {
                panic!("{target}: stable collision was not classified as partial")
            };
            {
                let state = session.authority.operations.lock().unwrap();
                assert!(!state.transients.contains_key(&first_id));
                let unpublished = &state.transients[&second_id];
                assert!(unpublished.phase == TransientEffectPhase::Live);
                assert!(unpublished.disposition == TransientEffectDisposition::Staged);
                assert!(unpublished.retained.is_none());
                assert_eq!(state.outstanding_effects, 1);
            }
            let [
                TransientPublicationMember::Published(file),
                TransientPublicationMember::Unpublished(stage),
            ] = <[_; 2]>::try_from(members).ok().expect("two members")
            else {
                panic!("{target}: partial members lost their order")
            };
            drop(file);
            discard_and_cancel(stage.discard());
            assert!(temporary.path().join("published.bin").is_file());
            assert!(collision.exists());
            if !directory {
                assert_eq!(std::fs::read(&collision).unwrap(), b"external collision");
            }
            revoke(session, root);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn grouped_zero_publication_returns_the_intact_no_effect_batch() {
        let (temporary, session, root) = fixture();
        let batch = sealed_batch(&root, &["zero-first.bin", "zero-second.bin"]);
        for name in ["zero-first.bin", "zero-second.bin"] {
            std::fs::write(temporary.path().join(name), name).unwrap();
        }
        let TransientPublicationBatchOutcome::NoEffect { batch, .. } = classify_collision(batch)
        else {
            panic!("zero-publication batch did not remain retryable as no effect")
        };
        assert_eq!(batch.len(), 2);
        for stage in batch.into_stages() {
            discard_and_cancel(stage.discard());
        }
        for name in ["zero-first.bin", "zero-second.bin"] {
            assert_eq!(
                std::fs::read(temporary.path().join(name)).unwrap(),
                name.as_bytes()
            );
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn dropped_mixed_pending_batch_retains_root_cleanable_authority() {
        let (_temporary, session, root) = fixture();
        let mut batch = sealed_batch(&root, &["pending-first.bin", "pending-second.bin"]);
        let (first_id, second_id) = (token_id(&batch.stages[0]), token_id(&batch.stages[1]));
        link(&mut batch.stages[0], &root, "pending-first.bin").unwrap();
        drop(TransientPublicationBatchObligation {
            error: io::Error::other("injected mixed pending classification"),
            batch: Some(batch),
        });
        assert_retained_transient(&session, first_id, TransientEffectDisposition::Published);
        {
            let state = session.authority.operations.lock().unwrap();
            let unpublished = &state.transients[&second_id];
            assert!(unpublished.phase == TransientEffectPhase::Abandoned);
            assert!(unpublished.disposition == TransientEffectDisposition::NoEffect);
            assert!(unpublished.retained.is_none());
            assert_eq!(state.outstanding_effects, 2);
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn sealed_stage_reads_and_seeks_within_its_admitted_size() {
        let (_temporary, session, root) = fixture();
        let mut stage = test_stage(&root, "readable.bin").unwrap();
        stage.write_all(b"0123456789").unwrap();
        let mut sealed = stage.seal().unwrap();
        let mut prefix = [0_u8; 4];
        sealed.read_exact(&mut prefix).unwrap();
        assert_eq!(&prefix, b"0123");
        assert_eq!(sealed.seek(SeekFrom::Current(2)).unwrap(), 6);
        let mut suffix = Vec::new();
        sealed.read_to_end(&mut suffix).unwrap();
        assert_eq!(suffix, b"6789");
        assert!(sealed.seek(SeekFrom::Start(11)).is_err());
        assert!(sealed.seek(SeekFrom::End(-11)).is_err());
        assert_eq!(sealed.stream_position().unwrap(), 10);
        assert_eq!(sealed.seek(SeekFrom::End(-3)).unwrap(), 7);
        let mut tail = [0_u8; 3];
        sealed.read_exact(&mut tail).unwrap();
        assert_eq!(&tail, b"789");
        assert_eq!(sealed.read(&mut prefix).unwrap(), 0);
        discard_and_cancel(sealed.discard());
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn unwind_after_native_link_retains_root_authority() {
        let (_temporary, session, root) = fixture();
        let (sealed, transition) = linked_test_stage(&root, "transition-unwind.bin");
        let (batched, batch) = linked_test_stage(&root, "batch-unwind.bin");
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _transition = TransientPublicationTransition::from_linked(sealed);
                panic!("injected unwind after native carrier extraction");
            }))
            .is_err()
        );
        assert!(
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let _batch = TransientPublicationBatch::new(vec![batched]).unwrap();
                panic!("injected unwind after publication classification");
            }))
            .is_err()
        );
        for id in [transition, batch] {
            assert_retained_transient(&session, id, TransientEffectDisposition::Published);
        }
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn held_transient_rejects_replacement_then_discards_unlinked_payload() {
        let (temporary, session, root) = fixture();
        let path = temporary.path().join("aba.bin");
        let mut stage = test_stage(&root, "aba.bin").unwrap();
        stage.write_all(b"held original").unwrap();
        let mut sealed = stage.seal().unwrap();
        link(&mut sealed, &root, "aba.bin").unwrap();

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"replacement").unwrap();
        {
            let operation = enter_transient_operation(sealed.stage.destination()).unwrap();
            assert!(validate_linked_publication(&sealed, &operation).is_err());
        }
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");

        std::fs::remove_file(&path).unwrap();
        let relink = link(&mut sealed, &root, "aba.bin")
            .expect_err("an unlinked published O_TMPFILE cannot be relinked");
        assert_eq!(relink.kind(), io::ErrorKind::NotFound);
        assert!(!path.exists());
        let TransientDiscardOutcome::Discarded(destination) = sealed.discard() else {
            panic!("unlinked transient discard remained pending")
        };
        drop(destination);
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepublication_collision_is_deferred_to_publish_and_preserves_the_stage() {
        let (temporary, session, root) = fixture();
        let destination = root
            .admit_transient_destination(LeafName::new("collision.bin").unwrap())
            .unwrap();
        std::fs::write(temporary.path().join("COLLISION.BIN"), b"user payload").unwrap();
        let TransientStageCreateOutcome::Created(stage) = destination.create_stage() else {
            panic!("collision prevented namespace-independent staging")
        };
        let batch = TransientPublicationBatch::new(vec![stage.seal().unwrap()]).unwrap();
        let TransientPublicationBatchOutcome::NoEffect { batch, .. } = batch.publish_create_new()
        else {
            panic!("portable collision did not preserve the stage")
        };
        discard_and_cancel(batch.into_stages().pop().unwrap().discard());
        assert_eq!(
            std::fs::read(temporary.path().join("COLLISION.BIN")).unwrap(),
            b"user payload",
        );
        revoke(session, root);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn external_preservation_keeps_replaced_publication_and_releases_root() {
        let temporary = crate::test_tempdir().unwrap();
        let root_path = temporary.path().join("profile");
        let parent = temporary.path().join("chosen");
        let displaced = temporary.path().join("displaced");
        std::fs::create_dir(&parent).unwrap();
        let session = acquire_test_root(&root_path);
        let admission = match session.admit_absolute_directory_authority_outside_root(&parent) {
            crate::AbsoluteDirectoryOutsideRootAdmission::Admitted(admission) => admission,
            _ => panic!("external exact parent"),
        };
        let destination = admission
            .admit_transient_destination(LeafName::new("saved.png").unwrap())
            .unwrap();
        let TransientStageCreateOutcome::Created(mut stage) = destination.create_stage() else {
            panic!("real transient file")
        };
        stage.write_all(b"complete original bytes").unwrap();
        let mut sealed = stage.seal().unwrap();
        platform::link_transient_file(
            sealed.stage.file.as_mut().unwrap(),
            admission.inner.directory.inner.handle(),
            OsStr::new("saved.png"),
        )
        .unwrap();
        std::fs::rename(parent.join("saved.png"), parent.join("original.png")).unwrap();
        std::fs::write(parent.join("saved.png"), b"replacement").unwrap();
        let batch = TransientPublicationBatch::new(vec![sealed]).unwrap();
        std::fs::rename(&parent, &displaced).unwrap();
        std::fs::create_dir(&parent).unwrap();
        std::fs::write(parent.join("saved.png"), b"new parent occupant").unwrap();
        let operation = session.authority.enter().unwrap();
        let TransientPublicationBatchOutcome::Pending(obligation) = classify_publication_batch(
            io::Error::other("publication acknowledgement interrupted after the real native link"),
            batch,
            &operation,
        ) else {
            panic!("changed binding is unknown")
        };
        drop(operation);
        assert!(obligation.acknowledge_preserved_singleton().is_ok());
        drop(admission);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        let reopened = acquire_test_root(&root_path);
        assert!(matches!(reopened.revoke(), RootRevokeOutcome::Revoked));
        for (path, bytes) in [
            (
                displaced.join("original.png"),
                b"complete original bytes".as_slice(),
            ),
            (displaced.join("saved.png"), b"replacement"),
            (parent.join("saved.png"), b"new parent occupant"),
        ] {
            assert_eq!(std::fs::read(path).unwrap(), bytes);
        }
    }

    #[test]
    fn external_preservation_does_not_waive_managed_transient_ownership() {
        let (_temporary, session, root) = fixture();
        let Some(stage) = test_stage(&root, "internal.bin") else {
            return revoke(session, root);
        };
        let obligation = TransientPublicationBatchObligation {
            error: io::Error::other("unclassified fixture"),
            batch: Some(TransientPublicationBatch::new(vec![stage.seal().unwrap()]).unwrap()),
        };
        let obligation = obligation
            .acknowledge_preserved_singleton()
            .expect_err("managed effect has no external preservation admission");
        assert_eq!(obligation.error().kind(), io::ErrorKind::PermissionDenied);
        let TransientPublicationBatchOutcome::NoEffect { batch, .. } = obligation.reconcile()
        else {
            panic!("unpublished managed stage")
        };
        discard_and_cancel(batch.into_stages().pop().unwrap().discard());
        revoke(session, root);
    }
}
