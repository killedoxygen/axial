use crate::{
    AUTHORITY_DRAINING, AUTHORITY_LIVE, CapabilityAuthority, CapabilityOperation, Directory,
    EntryKind, FileCapability, LeafName, MAX_DIRECTORY_LIST_ENTRIES, leaf_names_equivalent,
    platform, stale_capability,
};
use std::io;
use std::sync::{Arc, Weak};

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
    pub(super) phase: TransientEffectPhase,
    pub(super) disposition: TransientEffectDisposition,
}

struct TransientEffectToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

impl TransientEffectToken {
    fn reserve(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        destination: &TransientDestination,
    ) -> io::Result<Self> {
        if !Arc::ptr_eq(authority, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        if transient_destination_is_reserved(&state, destination) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "transient destination is reserved by another filesystem effect",
            ));
        }
        state.reserve_effect()?;
        let id = state.next_transient_id;
        let Some(next_id) = id.checked_add(1) else {
            state.outstanding_effects -= 1;
            return Err(io::Error::other("transient effect id overflowed"));
        };
        state.next_transient_id = next_id;
        assert!(
            state
                .transients
                .insert(
                    id,
                    TransientEffectRecord {
                        directory: destination.directory.clone(),
                        destination: destination.name.clone(),
                        identity: None,
                        phase: TransientEffectPhase::Reserved,
                        disposition: TransientEffectDisposition::Reserved,
                    },
                )
                .is_none()
        );
        Ok(Self {
            id,
            authority: Arc::downgrade(authority),
            armed: true,
        })
    }

    fn mark_live(&self, identity: platform::Identity) -> io::Result<()> {
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let mut state = authority.operations.lock().map_err(|_| {
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
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let mut state = authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        let record = state.transients.get_mut(&self.id).ok_or_else(stale_capability)?;
        if !matches!(record.phase, TransientEffectPhase::Reserved | TransientEffectPhase::Live) {
            return Err(stale_capability());
        }
        record.disposition = disposition;
        Ok(())
    }

    fn mark_disposition_on_drop(&self, disposition: TransientEffectDisposition) {
        let Some(authority) = self.authority.upgrade() else {
            return;
        };
        let mut state = authority
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(record) = state.transients.get_mut(&self.id) {
            record.disposition = disposition;
        }
    }

    fn settle(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let operation = authority.enter()?;
        self.settle_with(&operation)?;
        Ok(())
    }

    fn settle_with(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        if !Arc::ptr_eq(&authority, &operation.authority) {
            return Err(stale_capability());
        }
        authority.settle_transient_effect(self.id, operation)?;
        self.armed = false;
        Ok(())
    }

    fn abandon(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_transient_effect(self.id);
        }
    }
}

impl Drop for TransientEffectToken {
    fn drop(&mut self) {
        self.abandon();
    }
}

fn transient_destination_is_reserved(
    state: &crate::OperationState,
    destination: &TransientDestination,
) -> bool {
    if state.unsettled_moves != 0
        || state.file_parks_checked_out != 0
        || state.directory_parks_checked_out != 0
    {
        return true;
    }
    let conflicts_with_candidate = |directory: &Directory, name: &LeafName| {
        directory.inner.identity == destination.directory.inner.identity
            && leaf_names_equivalent(name.as_os_str(), destination.name.as_os_str())
    };
    state
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
            directory_has_physical_ancestor(&destination.directory, record.identity)
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

fn directory_has_physical_ancestor(
    directory: &Directory,
    ancestor: platform::Identity,
) -> bool {
    let mut current = directory;
    loop {
        if current.inner.identity.physical == ancestor {
            return true;
        }
        let Some(parent) = current.inner.parent.as_ref() else {
            return false;
        };
        current = &parent.directory;
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

pub(super) fn transient_directory_identity_is_reserved(
    state: &crate::OperationState,
    identity: platform::Identity,
) -> bool {
    state.transients.values().any(|record| {
        directory_has_physical_ancestor(&record.directory, identity)
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
            matches!(
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
        let result = match (record.disposition, record.identity) {
            (TransientEffectDisposition::NoEffect, _) => Ok(()),
            (
                TransientEffectDisposition::Published
                | TransientEffectDisposition::Indeterminate,
                Some(identity),
            ) => validate_terminal_publication(&record, identity, &operation),
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
    identity: platform::Identity,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    let destination = TransientDestination {
        directory: record.directory.clone(),
        name: record.destination.clone(),
    };
    let validate = || {
        record.directory.validate(operation)?;
        if platform::file_binding_state(
            &record.directory.inner.handle,
            record.destination.as_os_str(),
            identity,
        )? != platform::BindingState::Exact
            || platform::exact_file_link_count(
                &record.directory.inner.handle,
                record.destination.as_os_str(),
                identity,
            )? != Some(1)
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
pub struct TransientDestination {
    directory: Directory,
    name: LeafName,
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
        if let Err(error) = validate_portable_destination_with_operation(
            &self,
            true,
            &operation,
        ) {
            return TransientStageCreateOutcome::NoEffect {
                error,
                destination: self,
            };
        }
        let mut token = match TransientEffectToken::reserve(&authority, &operation, &self) {
            Ok(token) => token,
            Err(error) => {
                return TransientStageCreateOutcome::NoEffect {
                    error,
                    destination: self,
                };
            }
        };

        if let Err(error) = self.directory.validate(&operation) {
            return match token
                .mark_disposition(TransientEffectDisposition::NoEffect)
                .and_then(|()| token.settle_with(&operation))
            {
                Ok(()) => TransientStageCreateOutcome::NoEffect {
                    error,
                    destination: self,
                },
                Err(cleanup) => {
                    TransientStageCreateOutcome::Pending(TransientCreationObligation {
                        error: io::Error::other(format!(
                            "transient destination validation failed: {error}; registry settlement remains pending: {cleanup}"
                        )),
                        state: Some(TransientCreationState::Reservation {
                            destination: self,
                            token,
                        }),
                    })
                }
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
                match token
                    .mark_disposition(TransientEffectDisposition::NoEffect)
                    .and_then(|()| token.settle_with(&operation))
                {
                    Ok(()) => TransientStageCreateOutcome::NoEffect {
                        error,
                        destination: self,
                    },
                    Err(cleanup) => TransientStageCreateOutcome::Pending(
                        TransientCreationObligation {
                            error: io::Error::other(format!(
                                "transient creation failed: {error}; cleanup remains unsettled: {cleanup}"
                            )),
                            state: Some(TransientCreationState::Reservation {
                                destination: self,
                                token,
                            }),
                        },
                    ),
                }
            }
        }
    }
}

impl Directory {
    pub fn admit_transient_destination(
        &self,
        name: LeafName,
    ) -> io::Result<TransientDestination> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let destination = TransientDestination {
            directory: self.clone(),
            name,
        };
        validate_portable_destination_with_operation(&destination, true, &operation)?;
        let state = authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if transient_destination_is_reserved(&state, &destination) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "transient destination conflicts with an unsettled filesystem effect",
            ));
        }
        Ok(destination)
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
    Reservation {
        destination: TransientDestination,
        token: TransientEffectToken,
    },
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
            TransientCreationState::Reservation {
                destination,
                mut token,
            } => {
                let result = token
                    .mark_disposition(TransientEffectDisposition::NoEffect)
                    .and_then(|()| token.settle());
                match result {
                    Ok(()) => TransientStageCreateOutcome::NoEffect {
                        error: self.error,
                        destination,
                    },
                    Err(error) => TransientStageCreateOutcome::Pending(Self {
                        error,
                        state: Some(TransientCreationState::Reservation {
                            destination,
                            token,
                        }),
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
        Ok(TransientStageSealed { stage: self })
    }

    pub fn discard(mut self) -> TransientDiscardOutcome {
        let operation = match enter_transient_operation(&self.destination) {
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
                let mut token = self
                    .token
                    .take()
                    .expect("live transient stage retains its effect token");
                match token
                    .mark_disposition(TransientEffectDisposition::NoEffect)
                    .and_then(|()| token.settle_with(&operation))
                {
                    Ok(()) => TransientDiscardOutcome::Discarded,
                    Err(error) => TransientDiscardOutcome::Pending(
                        TransientDiscardObligation {
                            error,
                            state: Some(TransientDiscardState::Registry(token)),
                        },
                    ),
                }
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
        let cleanup = platform::discard_transient_file(file, self.identity);
        let cleanup_complete = match cleanup {
            Ok(()) => true,
            Err(platform::DiscardTransientFileError::Retained { file, .. }) => {
                drop(file);
                false
            }
        };
        let disposition = match topology {
            _ if cleanup_complete => TransientEffectDisposition::NoEffect,
            Ok(platform::TransientPublicationState::Published) => {
                TransientEffectDisposition::Published
            }
            _ => TransientEffectDisposition::Indeterminate,
        };
        if let Some(token) = self.token.as_ref() {
            token.mark_disposition_on_drop(disposition);
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

fn settle_linked_stage(
    mut sealed: TransientStageSealed,
    operation: &CapabilityOperation,
) -> TransientPublicationOutcome {
    if let Err(error) = validate_linked_publication(&sealed, operation) {
        return TransientPublicationOutcome::Pending(TransientPublicationObligation {
            error,
            state: Some(TransientPublicationState::Linked(sealed)),
        });
    }
    let directory = &sealed.stage.destination.directory;
    if let Err(error) = platform::sync_directory(&directory.inner.handle) {
        return TransientPublicationOutcome::Pending(TransientPublicationObligation {
            error,
            state: Some(TransientPublicationState::Linked(sealed)),
        });
    }
    if let Err(error) = validate_linked_publication(&sealed, operation) {
        return TransientPublicationOutcome::Pending(TransientPublicationObligation {
            error,
            state: Some(TransientPublicationState::Linked(sealed)),
        });
    }

    let file = sealed
        .stage
        .file
        .take()
        .expect("linked transient stage retains its file");
    let destination = TransientDestination {
        directory: sealed.stage.destination.directory.clone(),
        name: sealed.stage.destination.name.clone(),
    };
    match platform::finish_transient_publication(
        file,
        &destination.directory.inner.handle,
        destination.name.as_os_str(),
        sealed.stage.identity,
    ) {
        Ok(handle) => {
            let final_validation = (|| {
                let identity = platform::file_identity(&handle)?;
                if identity != sealed.stage.identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "published transient identity changed before effect settlement",
                    ));
                }
                validate_exact_publication(&sealed, operation)
            })();
            let mut token = sealed
                .stage
                .token
                .take()
                .expect("published transient stage retains its effect token");
            if let Err(error) = final_validation {
                drop(handle);
                let error = match token.mark_disposition(TransientEffectDisposition::Published) {
                    Ok(()) => error,
                    Err(registry) => io::Error::other(format!(
                        "transient publication validation failed: {error}; registry classification failed: {registry}"
                    )),
                };
                return TransientPublicationOutcome::Pending(
                    TransientPublicationObligation {
                        error,
                        state: Some(TransientPublicationState::Published {
                            destination,
                            identity: sealed.stage.identity,
                            token,
                        }),
                    },
                );
            }
            if let Err(error) = token
                .mark_disposition(TransientEffectDisposition::Published)
                .and_then(|()| token.settle_with(operation))
            {
                drop(handle);
                return TransientPublicationOutcome::Pending(
                    TransientPublicationObligation {
                        error,
                        state: Some(TransientPublicationState::Published {
                            destination,
                            identity: sealed.stage.identity,
                            token,
                        }),
                    },
                );
            }
            let file = FileCapability::new(
                handle,
                sealed.stage.identity,
                destination.directory.clone(),
                destination.name.clone(),
                destination.directory.inner.authority.clone(),
            );
            TransientPublicationOutcome::Published(file)
        }
        Err(platform::FinishTransientPublicationError::Retained { error, file }) => {
            sealed.stage.file = Some(file);
            TransientPublicationOutcome::Pending(TransientPublicationObligation {
                error,
                state: Some(TransientPublicationState::Linked(sealed)),
            })
        }
    }
}

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
    if platform::transient_publication_state(
        file,
        &destination.directory.inner.handle,
        destination.name.as_os_str(),
        sealed.stage.identity,
    )? != platform::TransientPublicationState::Published
        || !validate_portable_destination_with_operation(destination, false, operation)?
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "linked transient publication lost its exact destination topology",
        ));
    }
    destination.directory.validate(operation)
}

fn validate_exact_publication(
    sealed: &TransientStageSealed,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    validate_exact_destination(&sealed.stage.destination, sealed.stage.identity, operation)
}

fn validate_exact_destination(
    destination: &TransientDestination,
    identity: platform::Identity,
    operation: &CapabilityOperation,
) -> io::Result<()> {
    destination.directory.validate(operation)?;
    if platform::file_binding_state(
        &destination.directory.inner.handle,
        destination.name.as_os_str(),
        identity,
    )? != platform::BindingState::Exact
        || platform::exact_file_link_count(
            &destination.directory.inner.handle,
            destination.name.as_os_str(),
            identity,
        )? != Some(1)
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
    destination.directory.validate(operation)?;
    let listing = platform::entries(
        &destination.directory.inner.handle,
        MAX_DIRECTORY_LIST_ENTRIES,
    )?;
    destination.directory.validate(operation)?;
    if !listing.complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "transient destination inventory exceeded its bound",
        ));
    }
    let mut exact = false;
    for (name, kind) in listing.entries {
        if !leaf_names_equivalent(&name, destination.name.as_os_str()) {
            continue;
        }
        if name != destination.name.as_os_str() || exact {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "transient destination has a portable alias",
            ));
        }
        if kind != EntryKind::File {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "transient destination is not a regular file",
            ));
        }
        exact = true;
    }
    if require_vacant && exact {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "transient destination is already occupied",
        ));
    }
    Ok(exact)
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
    Published {
        destination: TransientDestination,
        identity: platform::Identity,
        token: TransientEffectToken,
    },
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

impl TransientPublicationObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
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
                            error: self.error,
                            stage,
                        }
                    }
                    _ => TransientPublicationOutcome::Pending(TransientPublicationObligation {
                        error: self.error,
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
            TransientPublicationState::Published {
                destination,
                identity,
                mut token,
            } => {
                let operation = match enter_transient_operation(&destination) {
                    Ok(operation) => operation,
                    Err(error) => {
                        return TransientPublicationOutcome::Pending(
                            TransientPublicationObligation {
                                error,
                                state: Some(TransientPublicationState::Published {
                                    destination,
                                    identity,
                                    token,
                                }),
                            },
                        );
                    }
                };
                let file = (|| {
                    validate_exact_destination(&destination, identity, &operation)?;
                    let handle = platform::open_file(
                        &destination.directory.inner.handle,
                        destination.name.as_os_str(),
                    )?;
                    if platform::file_identity(&handle)? != identity {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "published transient identity changed during reconciliation",
                        ));
                    }
                    validate_exact_destination(&destination, identity, &operation)?;
                    Ok(FileCapability::new(
                        handle,
                        identity,
                        destination.directory.clone(),
                        destination.name.clone(),
                        destination.directory.inner.authority.clone(),
                    ))
                })();
                match file {
                    Ok(file) => match token
                        .mark_disposition(TransientEffectDisposition::Published)
                        .and_then(|()| token.settle_with(&operation))
                    {
                        Ok(()) => TransientPublicationOutcome::Published(file),
                        Err(error) => TransientPublicationOutcome::Pending(
                            TransientPublicationObligation {
                                error,
                                state: Some(TransientPublicationState::Published {
                                    destination,
                                    identity,
                                    token,
                                }),
                            },
                        ),
                    },
                    Err(error) => TransientPublicationOutcome::Pending(TransientPublicationObligation {
                        error,
                        state: Some(TransientPublicationState::Published {
                            destination,
                            identity,
                            token,
                        }),
                    }),
                }
            }
        }
    }
}

#[must_use = "transient discard outcomes must retain failed cleanup authority"]
pub enum TransientDiscardOutcome {
    Discarded,
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
    Registry(TransientEffectToken),
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
            TransientDiscardState::Registry(mut token) => match token
                .mark_disposition(TransientEffectDisposition::NoEffect)
                .and_then(|()| token.settle())
            {
                Ok(()) => TransientDiscardOutcome::Discarded,
                Err(error) => TransientDiscardOutcome::Pending(Self {
                    error,
                    state: Some(TransientDiscardState::Registry(token)),
                }),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{RootRevokeOutcome, RootSession, RootSessionAcquireOutcome};
    use std::ffi::OsStr;

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
    fn prepublication_collision_preserves_the_stage() {
        let temporary = tempfile::tempdir().expect("temporary transient root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root directory");
        let destination = root
            .admit_transient_destination(LeafName::new("collision.bin").expect("collision leaf"))
            .expect("collision destination admission");
        std::fs::write(temporary.path().join("COLLISION.BIN"), b"user payload")
            .expect("portable collision injection");
        match destination.create_stage() {
            TransientStageCreateOutcome::NoEffect { destination, .. } => {
                assert_eq!(destination.name().as_os_str(), OsStr::new("collision.bin"));
            }
            TransientStageCreateOutcome::Created(stage) => {
                panic!("portable collision unexpectedly created a stage: {stage:?}")
            }
            TransientStageCreateOutcome::Pending(obligation) => {
                panic!("collision admission remained pending: {}", obligation.error())
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
