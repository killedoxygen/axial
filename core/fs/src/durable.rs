use super::*;

pub(crate) struct LiveReplacementCarrier {
    pub(crate) registration: RecoveryRegistration,
    pub(crate) handle: File,
    pub(crate) identity: platform::Identity,
    pub(crate) receipt: (u64, platform::FileStamp),
    pub(crate) proof: recovery::RecoveryFileProof,
}

#[must_use = "durable replacement outcomes must be settled"]
pub enum DurableReplaceOutcome {
    Replaced,
    NoEffect(io::Error),
    AppliedUnverified(DurableReplaceObligation),
}

impl_redacted_debug!(DurableReplaceOutcome);

#[must_use = "durable replacement obligations must be reconciled"]
pub struct DurableReplaceObligation {
    pub(crate) error: io::Error,
    pub(crate) state: Option<Box<DurableReplaceState>>,
}

impl_redacted_debug!(DurableReplaceObligation);

impl DurableReplaceObligation {
    pub fn reconcile(mut self) -> DurableReplaceOutcome {
        settle_durable_replace(
            *self
                .state
                .take()
                .expect("durable replacement obligation retains its owner"),
        )
    }
}

impl_error_accessor!(DurableReplaceObligation);

pub(crate) enum DurableReplaceState {
    Creating {
        cause: io::Error,
        obligation: FileCreateObligation,
    },
    Discarding {
        cause: io::Error,
        obligation: StageDiscardObligation,
    },
    Replaying {
        parent: Directory,
        replay: recovery::replay::RecoveryReplay,
    },
    Restoring(ReplacementRestore),
}

pub(crate) struct ReplacementRestore {
    pub(crate) parent: Directory,
    pub(crate) journal: Option<Box<RecoveryJournal>>,
    pub(crate) applied: bool,
}

impl Drop for ReplacementRestore {
    fn drop(&mut self) {
        if self.journal.is_some() {
            std::process::abort();
        }
    }
}

pub(crate) fn admit_durable_destination(
    destination: &ReplaceDestination,
    operation: &CapabilityOperation,
) -> io::Result<(Directory, LeafName, Option<recovery::RecoveryFileProof>)> {
    let (parent, name, existing) = match destination {
        ReplaceDestination::Vacant { parent, name } => (parent, name, None),
        ReplaceDestination::Existing(request) => {
            request
                .file
                .validate_bound_to(&request.file.parent, operation)?;
            request.validate_revision(operation)?;
            (&request.file.parent, &request.file.name, Some(request))
        }
        ReplaceDestination::Preserved(_) => {
            return Err(identity_changed(
                "preserved replacement destination requires fresh content admission",
            ));
        }
    };
    parent.validate(operation)?;
    let before = platform::directory_revision(parent.inner.handle())?;
    let listing = platform::entries(parent.inner.handle(), MAX_DIRECTORY_LIST_ENTRIES)?;
    if !listing.complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "replacement destination name-class scan exceeded its bound",
        ));
    }
    let mut matches = listing
        .entries
        .iter()
        .filter(|(candidate, _)| leaf_names_equivalent(candidate, name.as_os_str()));
    let matched = matches.next();
    if matches.next().is_some()
        || matched.is_some_and(|(actual, _)| actual.as_os_str() != name.as_os_str())
    {
        return Err(identity_changed(
            "replacement destination acquired a portable alias",
        ));
    }
    match (existing, matched) {
        (None, None) => {}
        (Some(request), Some((_, EntryKind::File)))
            if platform::file_binding_state(
                parent.inner.handle(),
                name.as_os_str(),
                request.file.identity,
            )? == platform::BindingState::Exact => {}
        (None, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "replacement destination is occupied",
            ));
        }
        _ => return Err(identity_changed("replacement destination changed identity")),
    }
    let proof = existing
        .map(|request| {
            let observed = recovery::replay::prove_file(
                parent.inner.handle(),
                name.as_os_str(),
                &request.file.handle,
                request.file.identity,
            )?;
            if observed.size != request.expected.revision.size
                || observed.sha256 != request.expected.sha256
            {
                return Err(identity_changed("replacement destination changed content"));
            }
            request.validate_revision(operation)?;
            Ok(observed)
        })
        .transpose()?;
    if platform::directory_revision(parent.inner.handle())? != before {
        return Err(identity_changed(
            "replacement destination changed during admission",
        ));
    }
    parent.validate(operation)?;
    Ok((parent.clone(), name.clone(), proof))
}

pub(crate) fn discard_durable_stage(
    cause: io::Error,
    discard: StageDiscardOutcome,
) -> DurableReplaceOutcome {
    match discard {
        StageDiscardOutcome::Discarded => DurableReplaceOutcome::NoEffect(cause),
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            settle_durable_replace(DurableReplaceState::Discarding { cause, obligation })
        }
    }
}

/// Checks the sealed replacement out of the live journal together with its exclusive stage
/// carrier, so the recovery engine that replays it after a crash also completes it now.
pub(crate) fn begin_live_replacement(
    parent: &Directory,
    sealed: SealedStagedFile,
) -> Result<recovery::replay::RecoveryReplay, (io::Error, SealedStagedFile)> {
    let authority = match parent.authority() {
        Ok(authority) => authority,
        Err(error) => return Err((error, sealed)),
    };
    let checked = (|| -> io::Result<_> {
        let operation = authority.enter()?;
        parent.validate(&operation)?;
        sealed.file.validate(&operation)?;
        sealed
            .file
            .validate_revision_in(&operation, &sealed.revision)?;
        let proof = recovery::replay::prove_file(
            parent.inner.handle(),
            sealed.file.name.as_os_str(),
            &sealed.file.handle,
            sealed.file.identity,
        )?;
        let receipt = platform::file_receipt_fields(&sealed.file.handle)?;
        if proof.size != sealed.revision.size
            || receipt != (sealed.revision.size, sealed.revision.stamp)
        {
            return Err(identity_changed(
                "durable replacement stage changed identity",
            ));
        }
        let mut state = authority.state()?;
        let stage = state
            .stages
            .get(&sealed.token.id)
            .ok_or_else(stale_capability)?;
        let registration = stage.recovery.ok_or_else(stale_capability)?;
        let record = live_recovery_record(&state, registration)?;
        if state.phase != AUTHORITY_LIVE
            || !sealed.token.armed
            || sealed.token.authority.as_ptr() != Arc::as_ptr(&authority)
            || stage.phase != StageRegistryPhase::Sealed
            || stage.carrier != StageCarrierState::Live
            || stage.identity != sealed.file.identity
            || stage.parent.inner.identity != parent.inner.identity
            || stage.name != sealed.file.name
            || stage.promotion.is_some()
            || record.phase != RecoveryPhase::StageSealed
            || record.new != Some(proof)
            || record.destination_parent != recovery_parent_components(parent)?
            || state.recovery.records().count() != 1
            || state.outstanding_effects == 0
            || platform::file_binding_state(
                parent.inner.handle(),
                sealed.file.name.as_os_str(),
                sealed.file.identity,
            )? != platform::BindingState::Exact
        {
            return Err(stale_capability());
        }
        let journal = state.recovery.take_for_replay()?;
        Ok((state, operation, registration, proof, receipt, journal))
    })();
    let (mut state, operation, registration, proof, receipt, journal) = match checked {
        Ok(checked) => checked,
        Err(error) => return Err((error, sealed)),
    };
    let SealedStagedFile {
        file,
        mut token,
        revision: _,
        published: _,
    } = sealed;
    let cleanup = state
        .stages
        .remove(&token.id)
        .expect("validated replacement stage remains registered")
        .cleanup;
    token.armed = false;
    state.release_effect(&operation);
    drop(state);
    drop(cleanup);
    drop(token);
    Ok(recovery::replay::RecoveryReplay::from_live_carrier(
        journal,
        LiveReplacementCarrier {
            registration,
            handle: file.handle,
            identity: file.identity,
            receipt,
            proof,
        },
    ))
}

pub(crate) fn retain_durable_replace(
    error: io::Error,
    state: DurableReplaceState,
) -> DurableReplaceOutcome {
    DurableReplaceOutcome::AppliedUnverified(DurableReplaceObligation {
        error,
        state: Some(Box::new(state)),
    })
}

pub(crate) fn settle_durable_replace(mut state: DurableReplaceState) -> DurableReplaceOutcome {
    loop {
        state = match state {
            DurableReplaceState::Creating { cause, obligation } => match obligation.reconcile() {
                FileCreateResolution::Created(staged) => {
                    return discard_durable_stage(cause, staged.discard());
                }
                FileCreateResolution::NoEffect(_) => {
                    return DurableReplaceOutcome::NoEffect(cause);
                }
                FileCreateResolution::Indeterminate(obligation) => {
                    let error = copy_io_error(obligation.error());
                    return retain_durable_replace(
                        error,
                        DurableReplaceState::Creating { cause, obligation },
                    );
                }
            },
            DurableReplaceState::Discarding { cause, obligation } => match obligation.reconcile() {
                StageDiscardResolution::Discarded => {
                    return DurableReplaceOutcome::NoEffect(cause);
                }
                StageDiscardResolution::Indeterminate(obligation) => {
                    let error = copy_io_error(obligation.error());
                    return retain_durable_replace(
                        error,
                        DurableReplaceState::Discarding { cause, obligation },
                    );
                }
            },
            DurableReplaceState::Replaying { parent, replay } => {
                let authority = match parent.authority() {
                    Ok(authority) => authority,
                    Err(error) => {
                        return retain_durable_replace(
                            error,
                            DurableReplaceState::Replaying { parent, replay },
                        );
                    }
                };
                match replay.resume_live(&authority.root, &authority.lease) {
                    Ok((journal, applied)) => DurableReplaceState::Restoring(ReplacementRestore {
                        parent,
                        journal: Some(Box::new(journal)),
                        applied,
                    }),
                    Err((error, replay)) => {
                        return retain_durable_replace(
                            error,
                            DurableReplaceState::Replaying { parent, replay },
                        );
                    }
                }
            }
            DurableReplaceState::Restoring(mut restore) => {
                let restored = restore.parent.authority().and_then(|authority| {
                    let mut state = authority.state()?;
                    state.recovery.restore_after_replay(
                        *restore
                            .journal
                            .take()
                            .expect("replacement restore retains its journal"),
                    );
                    Ok(())
                });
                if let Err(error) = restored {
                    return retain_durable_replace(error, DurableReplaceState::Restoring(restore));
                }
                return if restore.applied {
                    DurableReplaceOutcome::Replaced
                } else {
                    DurableReplaceOutcome::NoEffect(identity_changed(
                        "durable replacement destination changed before publication",
                    ))
                };
            }
        };
    }
}
