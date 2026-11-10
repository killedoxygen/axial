use super::*;

pub(crate) fn live_recovery_record(
    state: &OperationState,
    registration: RecoveryRegistration,
) -> io::Result<RecoveryRecord> {
    state
        .recovery
        .record(registration)
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid recovery record"))
}

pub(crate) fn selected_recovery_stage_record(
    authority: &CapabilityAuthority,
    registration: Option<RecoveryRegistration>,
) -> io::Result<Option<RecoveryRecord>> {
    let Some(registration) = registration else {
        return Ok(None);
    };
    let state = authority.state()?;
    Ok(Some(live_recovery_record(&state, registration)?))
}

pub(crate) fn prepare_recovery_stage_removal(
    authority: &CapabilityAuthority,
    stage: &StageRecord,
) -> io::Result<Option<RecoveryRecord>> {
    let Some(registration) = stage.recovery else {
        return Ok(None);
    };
    let mut state = authority.state()?;
    let current = live_recovery_record(&state, registration)?;
    let expected_stage = recovery_stage_leaf(current.operation_id);
    if recovery_parent_components(&stage.parent)? != current.destination_parent
        || stage.name.as_os_str() != OsStr::new(expected_stage.as_str())
        || stage.promotion.as_ref().is_some_and(|promotion| {
            promotion.destination.parent.inner.identity != stage.parent.inner.identity
                || promotion.destination.name.as_os_str()
                    != OsStr::new(current.destination_leaf.as_str())
        })
    {
        return Err(stale_capability());
    }
    match current.phase {
        RecoveryPhase::StagePrepared if current.new.is_none() => Ok(Some(current)),
        RecoveryPhase::RemovePrepared if current.new.is_some() => Ok(Some(current)),
        RecoveryPhase::StageSealed | RecoveryPhase::PublishPrepared if current.new.is_some() => {
            let mut intended = current;
            intended.phase = RecoveryPhase::RemovePrepared;
            state
                .recovery
                .advance(&authority.lease, registration, intended.clone())?;
            Ok(Some(intended))
        }
        _ => Err(stale_capability()),
    }
}

pub(crate) fn clear_recovery(
    lease: &platform::LeaseHandle,
    state: &mut OperationState,
    registration: RecoveryRegistration,
) -> io::Result<()> {
    state.recovery.clear(lease, registration)?;
    state
        .recovery_orphans
        .retain(|orphan| orphan.registration != registration);
    Ok(())
}

#[cfg(windows)]
pub(crate) fn retain_recovery_orphan(
    state: &mut OperationState,
    registration: RecoveryRegistration,
    parent: &Directory,
    files: Vec<platform::Identity>,
) {
    if let Some(orphan) = state
        .recovery_orphans
        .iter_mut()
        .find(|orphan| orphan.registration == registration)
    {
        orphan.files = files;
        return;
    }
    let mut ancestors = Vec::new();
    let mut current = Some(parent);
    while let Some(directory) = current {
        ancestors.push(directory.inner.identity.physical);
        current = directory
            .inner
            .parent
            .as_ref()
            .map(|binding| &binding.directory);
    }
    ancestors.reverse();
    state.recovery_orphans.push(RecoveryOrphan {
        registration,
        ancestors,
        files,
    });
}

pub(crate) fn settle_removed_recovery(
    authority: &CapabilityAuthority,
    operation: &CapabilityOperation,
    registration: RecoveryRegistration,
    parent: &Directory,
    target: Option<platform::Identity>,
    expected: &RecoveryRecord,
) -> io::Result<()> {
    if !std::ptr::eq(authority, Arc::as_ptr(&operation.authority)) {
        return Err(stale_capability());
    }
    parent.validate(operation)?;
    #[cfg(unix)]
    {
        let _ = target;
        let state = authority.state()?;
        match state.recovery.record(registration) {
            None => return Ok(()),
            Some(current) if current == expected => {}
            Some(_) => return Err(stale_capability()),
        }
        drop(state);
        platform::sync_publication_directory(parent.inner.handle())?;
        parent.validate(operation)?;
        let mut state = authority.state()?;
        parent.validate(operation)?;
        if state.recovery.record(registration) != Some(expected) {
            return Err(stale_capability());
        }
        clear_recovery(&authority.lease, &mut state, registration)
    }
    #[cfg(windows)]
    {
        parent.validate(operation)?;
        let mut state = authority.state()?;
        parent.validate(operation)?;
        let current = live_recovery_record(&state, registration)?;
        let mut successor = expected.clone();
        successor.phase = RecoveryPhase::RemoveCommitted;
        match (target, expected.phase) {
            (Some(_), RecoveryPhase::PublishPrepared) if current == *expected => {
                state
                    .recovery
                    .advance(&authority.lease, registration, successor)?;
            }
            (Some(_), RecoveryPhase::PublishPrepared) if current == successor => {}
            (Some(_), RecoveryPhase::RemoveCommitted) if current == *expected => {}
            (None, RecoveryPhase::StagePrepared | RecoveryPhase::RemovePrepared)
                if current == *expected => {}
            _ => return Err(stale_capability()),
        }
        parent.validate(operation)?;
        if target.is_none() {
            return clear_recovery(&authority.lease, &mut state, registration);
        }
        retain_recovery_orphan(
            &mut state,
            registration,
            parent,
            target.into_iter().collect(),
        );
        Ok(())
    }
}

pub(crate) fn settle_removed_recovery_stage(
    authority: &CapabilityAuthority,
    operation: &CapabilityOperation,
    record: &StageRecord,
    expected: Option<&RecoveryRecord>,
) -> io::Result<()> {
    let (registration, expected) = match (record.recovery, expected) {
        (None, None) => return Ok(()),
        (Some(registration), Some(expected))
            if registration.operation_id == expected.operation_id =>
        {
            (registration, expected)
        }
        _ => return Err(stale_capability()),
    };
    let expected_stage = recovery_stage_leaf(expected.operation_id);
    if record.name.as_os_str() != OsStr::new(expected_stage.as_str())
        || recovery_parent_components(&record.parent)? != expected.destination_parent
    {
        return Err(stale_capability());
    }
    if matches!(
        expected.phase,
        RecoveryPhase::StagePrepared | RecoveryPhase::RemovePrepared
    ) {
        return settle_removed_recovery(
            authority,
            operation,
            registration,
            &record.parent,
            None,
            expected,
        );
    }
    let published = record.promotion.as_ref().and_then(|promotion| {
        (promotion.destination.name.as_os_str() == OsStr::new(expected.destination_leaf.as_str())
            && recovery_parent_components(&promotion.destination.parent)
                .ok()
                .as_ref()
                == Some(&expected.destination_parent)
            && platform::file_binding_state(
                promotion.destination.parent.inner.handle(),
                promotion.destination.name.as_os_str(),
                record.identity,
            )
            .is_ok_and(|state| state == platform::BindingState::Exact))
        .then_some((&promotion.destination.parent, record.identity))
    });
    match (expected.phase, published) {
        (
            RecoveryPhase::PublishPrepared | RecoveryPhase::RemoveCommitted,
            Some((parent, identity)),
        ) => {
            let handle = platform::open_file(
                parent.inner.handle(),
                OsStr::new(expected.destination_leaf.as_str()),
            )?;
            let proof = recovery::replay::prove_file(
                parent.inner.handle(),
                OsStr::new(expected.destination_leaf.as_str()),
                &handle,
                identity,
            )?;
            if expected.new != Some(proof) {
                return Err(identity_changed(
                    "published recovery content changed before settlement",
                ));
            }
            settle_removed_recovery(
                authority,
                operation,
                registration,
                parent,
                Some(identity),
                expected,
            )
        }
        _ => Err(stale_capability()),
    }
}

pub(crate) fn settle_removed_recovery_stage_create(
    authority: &CapabilityAuthority,
    operation: &CapabilityOperation,
    record: &StageCreateRecord,
) -> io::Result<()> {
    match record.recovery {
        Some(registration) => {
            let expected = authority
                .state()?
                .recovery
                .record(registration)
                .cloned()
                .ok_or_else(stale_capability)?;
            settle_removed_recovery(
                authority,
                operation,
                registration,
                &record.parent,
                None,
                &expected,
            )
        }
        None => Ok(()),
    }
}

pub(crate) fn validate_recovery_stage_writable(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    file: &FileCapability,
    token: &StageToken,
) -> io::Result<()> {
    if !Arc::ptr_eq(authority, &operation.authority)
        || !token.armed
        || token.authority.as_ptr() != Arc::as_ptr(authority)
    {
        return Err(stale_capability());
    }
    let state = authority.state()?;
    let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
    if stage.phase != StageRegistryPhase::Writing
        || stage.carrier != StageCarrierState::Live
        || stage.identity != file.identity
        || stage.parent.inner.identity != file.parent.inner.identity
        || stage.name != file.name
    {
        return Err(stale_capability());
    }
    let Some(registration) = stage.recovery else {
        return Ok(());
    };
    let record = live_recovery_record(&state, registration)?;
    let expected_stage = recovery_stage_leaf(record.operation_id);
    if record.phase != RecoveryPhase::StagePrepared
        || record.new.is_some()
        || recovery_parent_components(&stage.parent)? != record.destination_parent
        || stage.name.as_os_str() != OsStr::new(expected_stage.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "durably sealed recovery stage is not writable",
        ));
    }
    Ok(())
}

pub(crate) fn stage_has_recovery(
    authority: &Arc<CapabilityAuthority>,
    token: &StageToken,
) -> io::Result<bool> {
    let state = authority.state()?;
    Ok(state
        .stages
        .get(&token.id)
        .ok_or_else(stale_capability)?
        .recovery
        .is_some())
}

/// Makes staged bytes durable and opens the shared read handle that becomes the
/// promoted capability, so on-access scanning stays out of serialized promotion.
pub(crate) fn admit_publication_bytes(file: &FileCapability) -> io::Result<File> {
    platform::sync_publication_file(&file.handle)?;
    let handle = platform::open_file(file.parent.inner.handle(), file.name.as_os_str())?;
    if platform::file_identity(&handle)? != file.identity {
        return Err(identity_changed(
            "staged file changed before read capability admission",
        ));
    }
    Ok(handle)
}

pub(crate) fn seal_recovery_stage(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    file: &FileCapability,
    revision: &FileRevision,
    token: &StageToken,
) -> io::Result<()> {
    if !Arc::ptr_eq(authority, &operation.authority)
        || !token.armed
        || token.authority.as_ptr() != Arc::as_ptr(authority)
    {
        return Err(stale_capability());
    }
    let registration = {
        let state = authority.state()?;
        let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
        if stage.phase != StageRegistryPhase::Writing
            || stage.carrier != StageCarrierState::Live
            || stage.identity != file.identity
            || stage.parent.inner.identity != file.parent.inner.identity
            || stage.name != file.name
        {
            return Err(stale_capability());
        }
        stage.recovery
    };
    let Some(registration) = registration else {
        return Ok(());
    };
    let proof = recovery::replay::prove_file(
        file.parent.inner.handle(),
        file.name.as_os_str(),
        &file.handle,
        file.identity,
    )?;
    file.validate_revision_in(operation, revision)?;
    if proof.size != revision.size {
        return Err(identity_changed("recoverable stage revision changed"));
    }
    let mut state = authority.state()?;
    let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
    if stage.phase != StageRegistryPhase::Writing
        || stage.carrier != StageCarrierState::Live
        || stage.identity != file.identity
        || stage.parent.inner.identity != file.parent.inner.identity
        || stage.name != file.name
        || stage.recovery != Some(registration)
    {
        return Err(stale_capability());
    }
    let parent = stage.parent.clone();
    let stage_name = stage.name.clone();
    let current = live_recovery_record(&state, registration)?;
    let expected_stage = recovery_stage_leaf(current.operation_id);
    if recovery_parent_components(&parent)? != current.destination_parent
        || stage_name.as_os_str() != OsStr::new(expected_stage.as_str())
    {
        return Err(stale_capability());
    }
    match current.phase {
        RecoveryPhase::StagePrepared if current.new.is_none() => {
            let destination = recovery_leaf(&current.destination_leaf)?;
            if state
                .namespace_footprint_is_reserved(FootprintQuery::leaves(&[(&parent, &destination)]))
            {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "recoverable publication destination is reserved",
                ));
            }
            let mut intended = current;
            intended.phase = RecoveryPhase::StageSealed;
            intended.new = Some(proof);
            state
                .recovery
                .advance(&authority.lease, registration, intended)
        }
        RecoveryPhase::StageSealed if current.new == Some(proof) => Ok(()),
        _ => Err(stale_capability()),
    }
}

pub(crate) fn validate_recovery_publication(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    token: &StageToken,
    file: &FileCapability,
    revision: &FileRevision,
    source: &Directory,
    destination: &Directory,
    destination_name: &LeafName,
    displaced: Option<&ParkedFile>,
) -> io::Result<Option<RecoveryRecord>> {
    if !Arc::ptr_eq(authority, &operation.authority) {
        return Err(stale_capability());
    }
    let state = authority.state()?;
    let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
    let Some(registration) = stage.recovery else {
        return Ok(None);
    };
    let mut expected = live_recovery_record(&state, registration)?;
    let stage_name = recovery_stage_leaf(expected.operation_id);
    let parent_components = recovery_parent_components(destination)?;
    let destination_spelling = destination_name.as_os_str().to_str();
    if displaced.is_some()
        || !matches!(
            expected.phase,
            RecoveryPhase::StageSealed | RecoveryPhase::PublishPrepared
        )
        || expected.old.is_some()
        || source.inner.identity != destination.inner.identity
        || source.inner.identity != stage.parent.inner.identity
        || file.identity != stage.identity
        || file.parent.inner.identity != stage.parent.inner.identity
        || file.name.as_os_str() != OsStr::new(stage_name.as_str())
        || expected.destination_parent != parent_components
        || destination_spelling != Some(expected.destination_leaf.as_str())
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "publication does not match its durable create-only destination",
        ));
    }
    let proof = recovery::replay::prove_file(
        source.inner.handle(),
        file.name.as_os_str(),
        &file.handle,
        file.identity,
    )?;
    if expected.new != Some(proof) || proof.size != revision.size {
        return Err(identity_changed(
            "publication does not match its durable sealed proof",
        ));
    }
    expected.phase = RecoveryPhase::PublishPrepared;
    Ok(Some(expected))
}

pub(crate) fn prepare_recovery_publication(
    lease: &platform::LeaseHandle,
    state: &mut OperationState,
    stage_id: u64,
    expected: Option<&RecoveryRecord>,
) -> io::Result<()> {
    let Some(registration) = state
        .stages
        .get(&stage_id)
        .ok_or_else(stale_capability)?
        .recovery
    else {
        return expected
            .is_none()
            .then_some(())
            .ok_or_else(stale_capability);
    };
    let expected = expected.ok_or_else(stale_capability)?;
    if expected.phase != RecoveryPhase::PublishPrepared {
        return Err(stale_capability());
    }
    let mut predecessor = expected.clone();
    predecessor.phase = RecoveryPhase::StageSealed;
    match state.recovery.record(registration) {
        Some(current) if current == expected => Ok(()),
        Some(current) if current == &predecessor => {
            state
                .recovery
                .advance(lease, registration, expected.clone())
        }
        _ => Err(stale_capability()),
    }
}

pub(crate) fn complete_recovery_publication(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    token: &StageToken,
    file: &FileCapability,
    revision: &FileRevision,
    destination: &Directory,
    destination_name: &LeafName,
) -> io::Result<()> {
    if !Arc::ptr_eq(authority, &operation.authority) {
        return Err(stale_capability());
    }
    let registration = {
        let state = authority.state()?;
        let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
        let promotion = stage.promotion.as_ref().ok_or_else(stale_capability)?;
        if stage.identity != file.identity
            || stage.parent.inner.identity != file.parent.inner.identity
            || promotion.destination.parent.inner.identity != destination.inner.identity
            || promotion.destination.name != *destination_name
        {
            return Err(stale_capability());
        }
        let Some(registration) = stage.recovery else {
            return Ok(());
        };
        if stage.parent.inner.identity != destination.inner.identity {
            return Err(stale_capability());
        }
        registration
    };
    let destination_parent = recovery_parent_components(destination)?;
    let destination_spelling = destination_name
        .as_os_str()
        .to_str()
        .ok_or_else(stale_capability)?;
    let proof = recovery::replay::prove_file(
        destination.inner.handle(),
        destination_name.as_os_str(),
        &file.handle,
        file.identity,
    )?;
    if proof.size != revision.size {
        return Err(identity_changed(
            "published recovery revision changed during completion",
        ));
    }
    let expected = {
        let state = authority.state()?;
        let stage = state.stages.get(&token.id).ok_or_else(stale_capability)?;
        let promotion = stage.promotion.as_ref().ok_or_else(stale_capability)?;
        if stage.recovery != Some(registration)
            || stage.identity != file.identity
            || stage.parent.inner.identity != file.parent.inner.identity
            || stage.parent.inner.identity != destination.inner.identity
            || promotion.destination.parent.inner.identity != destination.inner.identity
            || promotion.destination.name != *destination_name
        {
            return Err(stale_capability());
        }
        let Some(record) = state.recovery.record(registration) else {
            #[cfg(unix)]
            return Ok(());
            #[cfg(windows)]
            return Err(stale_capability());
        };
        let stage_name = recovery_stage_leaf(record.operation_id);
        if !matches!(
            record.phase,
            RecoveryPhase::PublishPrepared | RecoveryPhase::RemoveCommitted
        ) || record.old.is_some()
            || record.new != Some(proof)
            || record.destination_parent != destination_parent
            || record.destination_leaf.as_str() != destination_spelling
            || file.name.as_os_str() != OsStr::new(stage_name.as_str())
        {
            return Err(stale_capability());
        }
        record.clone()
    };
    settle_removed_recovery(
        authority,
        operation,
        registration,
        destination,
        Some(file.identity),
        &expected,
    )
}

pub(crate) fn recovery_parent_components(directory: &Directory) -> io::Result<Vec<RecoveryName>> {
    if !directory.is_managed_root_descendant() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "recoverable publication requires a managed root descendant",
        ));
    }
    let mut components = Vec::new();
    let mut current = directory;
    while let Some(DirectoryParent {
        directory: parent,
        name,
    }) = current.inner.parent.as_ref()
    {
        let name = name.to_str().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "recovery parent is not Unicode",
            )
        })?;
        components
            .push(RecoveryName::new_exact(name.to_owned()).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidData, "invalid recovery name")
            })?);
        current = parent;
    }
    components.reverse();
    Ok(components)
}

pub(crate) fn recovery_leaf(name: &RecoveryName) -> io::Result<LeafName> {
    LeafName::new(name.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid recovery name"))
}

pub(crate) fn preflight_recovery_create(
    parent: &Directory,
    stage: &LeafName,
    park: Option<&LeafName>,
) -> io::Result<()> {
    let listing = platform::entries(parent.inner.handle(), MAX_DIRECTORY_LIST_ENTRIES)?;
    if !listing.complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "recoverable publication preflight exceeded its bound",
        ));
    }
    if listing.entries.iter().any(|(candidate, _)| {
        leaf_names_equivalent(candidate, stage.as_os_str())
            || park.is_some_and(|park| leaf_names_equivalent(candidate, park.as_os_str()))
    }) {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "recoverable publication footprint is occupied",
        ));
    }
    Ok(())
}

pub(crate) fn reconcile_recovery_stage_create(
    authority: &Arc<CapabilityAuthority>,
    token: &mut StageCreateToken,
) -> io::Result<Option<(Directory, LeafName)>> {
    let mut state = authority.state()?;
    let (parent, stage, registration, intent) = {
        let header = state
            .stage_creations
            .get(&token.id)
            .ok_or_else(stale_capability)?;
        let Some(intent) = header.recovery_intent.clone() else {
            return Ok(None);
        };
        (
            header.parent.clone(),
            header.name.clone(),
            header.recovery.ok_or_else(stale_capability)?,
            intent,
        )
    };
    if state.recovery.record(registration) != Some(&intent) {
        let park = intent
            .old
            .is_some()
            .then(|| recovery_leaf(&recovery_park_leaf(intent.operation_id)))
            .transpose()?;
        preflight_recovery_create(&parent, &stage, park.as_ref())?;
        state
            .recovery
            .create_reserved(&authority.lease, registration, intent.clone())?;
    }
    state
        .stage_creations
        .get_mut(&token.id)
        .expect("recovery create carrier remains registered")
        .recovery_intent = None;
    Ok(Some((parent, stage)))
}
