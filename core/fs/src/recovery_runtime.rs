use crate::platform::{self, BindingState};
use crate::recovery::{
    MAX_RECOVERABLE_FILE_BYTES, RecoveryFileProof, RecoveryJournal, RecoveryName, RecoveryPhase,
    RecoveryRecord, RecoveryRegistration, recovery_park_leaf, recovery_stage_leaf,
};
use crate::{EntryKind, MAX_DIRECTORY_LIST_ENTRIES, identity_changed, leaf_names_equivalent};
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::ffi::OsStr;
use std::fs::File;
use std::io;

#[cfg(test)]
thread_local! {
    static REPLAY_PARENT_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
}

#[cfg(test)]
pub(crate) fn set_replay_parent_validation_hook(hook: impl FnOnce() + 'static) {
    REPLAY_PARENT_VALIDATION_HOOK.with(|slot| {
        assert!(slot.borrow_mut().replace(Box::new(hook)).is_none());
    });
}

#[cfg(test)]
fn run_replay_parent_validation_hook() {
    REPLAY_PARENT_VALIDATION_HOOK.with(|slot| {
        if let Some(hook) = slot.borrow_mut().take() {
            hook();
        }
    });
}

struct ObservedFile {
    handle: File,
    identity: platform::Identity,
    proof: Option<RecoveryFileProof>,
}

enum ObservedEntry {
    Absent,
    File(ObservedFile),
    Unowned,
    UnownedOccupied,
}

struct RetainedParentBinding {
    parent: platform::DirectoryHandle,
    parent_identity: platform::Identity,
    name: RecoveryName,
    child_identity: platform::Identity,
}

struct ReplayPlan {
    registration: RecoveryRegistration,
    record: RecoveryRecord,
    parent: platform::DirectoryHandle,
    parent_identity: platform::Identity,
    ancestors: Vec<platform::Identity>,
    parent_bindings: Vec<RetainedParentBinding>,
    stage: ObservedEntry,
    target: ObservedEntry,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn codec_error() -> io::Error {
    invalid("root recovery control contains an invalid record")
}

fn open_parent(
    root: &platform::RootGuard,
    components: &[RecoveryName],
) -> io::Result<(
    platform::DirectoryHandle,
    Vec<platform::Identity>,
    Vec<RetainedParentBinding>,
)> {
    let mut parent = platform::clone_root(root)?;
    let mut ancestors = vec![platform::directory_identity(&parent)?];
    let mut bindings = Vec::with_capacity(components.len());
    for component in components {
        let expected = OsStr::new(component.as_str());
        let listing = platform::entries(&parent, MAX_DIRECTORY_LIST_ENTRIES)?;
        if !listing.complete {
            return Err(invalid("recovery parent enumeration exceeded its bound"));
        }
        let mut matches = listing
            .entries
            .into_iter()
            .filter(|(name, _)| leaf_names_equivalent(name, expected));
        let Some((actual, kind)) = matches.next() else {
            return Err(invalid("recovery parent is absent"));
        };
        if matches.next().is_some() || actual != expected || kind != EntryKind::Directory {
            return Err(invalid("recovery parent is aliased or has the wrong type"));
        }
        let (child, child_identity) = platform::open_directory(&parent, expected)?;
        bindings.push(RetainedParentBinding {
            parent,
            parent_identity: *ancestors.last().expect("recovery parent retains its root"),
            name: component.clone(),
            child_identity,
        });
        parent = child;
        ancestors.push(child_identity);
    }
    Ok((parent, ancestors, bindings))
}

fn validate_parent_chain(root: &platform::RootGuard, plan: &ReplayPlan) -> io::Result<()> {
    platform::validate_root(root)?;
    let current_root = platform::clone_root(root)?;
    let root_identity = platform::directory_identity(&current_root)?;
    if plan.ancestors.first().copied() != Some(root_identity) {
        return Err(identity_changed("recovery root-relative ancestry changed"));
    }
    if let Some(first) = plan.parent_bindings.first()
        && platform::directory_identity(&first.parent)? != root_identity
    {
        return Err(identity_changed("recovery root handle changed"));
    }
    for (index, binding) in plan.parent_bindings.iter().enumerate() {
        if platform::directory_identity(&binding.parent)? != binding.parent_identity {
            return Err(identity_changed("recovery parent handle changed identity"));
        }
        let expected = OsStr::new(binding.name.as_str());
        let listing = platform::entries(&binding.parent, MAX_DIRECTORY_LIST_ENTRIES)?;
        if !listing.complete {
            return Err(invalid("recovery parent revalidation exceeded its bound"));
        }
        let mut matches = listing
            .entries
            .into_iter()
            .filter(|(name, _)| leaf_names_equivalent(name, expected));
        let Some((actual, kind)) = matches.next() else {
            return Err(identity_changed("recovery parent binding disappeared"));
        };
        if matches.next().is_some()
            || actual != expected
            || kind != EntryKind::Directory
            || platform::directory_binding_state(&binding.parent, expected, binding.child_identity)?
                != BindingState::Exact
        {
            return Err(identity_changed("recovery parent binding changed"));
        }
        let retained_child = plan
            .parent_bindings
            .get(index + 1)
            .map(|next| &next.parent)
            .unwrap_or(&plan.parent);
        if platform::directory_identity(retained_child)? != binding.child_identity {
            return Err(identity_changed("recovery retained parent chain changed"));
        }
    }
    if platform::directory_identity(&plan.parent)? != plan.parent_identity {
        return Err(identity_changed(
            "recovery destination parent changed identity",
        ));
    }
    Ok(())
}

pub(crate) fn prove_file(
    parent: &platform::DirectoryHandle,
    name: &OsStr,
    file: &File,
    identity: platform::Identity,
) -> io::Result<RecoveryFileProof> {
    let before = platform::file_receipt_fields(file)?;
    if before.0 > MAX_RECOVERABLE_FILE_BYTES {
        return Err(invalid("recovery file exceeds its size bound"));
    }
    let mut digest = Sha256::new();
    let mut bytes = [0; 64 * 1024];
    let mut offset = 0;
    while offset < before.0 {
        let length = usize::try_from((before.0 - offset).min(bytes.len() as u64))
            .map_err(|_| invalid("recovery read length overflowed"))?;
        let read = platform::read_at(file, &mut bytes[..length], offset)?;
        if read == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        digest.update(&bytes[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| invalid("recovery read offset overflowed"))?;
    }
    if platform::file_receipt_fields(file)? != before
        || platform::file_identity(file)? != identity
        || platform::file_binding_state(parent, name, identity)? != BindingState::Exact
    {
        return Err(identity_changed("recovery file changed while being proven"));
    }
    Ok(RecoveryFileProof {
        size: before.0,
        sha256: digest.finalize().into(),
    })
}

fn observe(
    parent: &platform::DirectoryHandle,
    name: &RecoveryName,
    prove: bool,
    recoverable_stage: bool,
) -> io::Result<ObservedEntry> {
    let expected = OsStr::new(name.as_str());
    let listing = platform::entries(parent, MAX_DIRECTORY_LIST_ENTRIES)?;
    if !listing.complete {
        return Err(invalid("recovery leaf enumeration exceeded its bound"));
    }
    let mut matches = listing
        .entries
        .into_iter()
        .filter(|(candidate, _)| leaf_names_equivalent(candidate, expected));
    let Some((actual, kind)) = matches.next() else {
        return Ok(ObservedEntry::Absent);
    };
    if matches.next().is_some() || actual != expected || kind != EntryKind::File {
        return Err(invalid("recovery leaf is aliased or has the wrong type"));
    }
    let mut handle = platform::open_file(parent, expected)?;
    let identity = platform::file_identity(&handle)?;
    if platform::file_binding_state(parent, expected, identity)? != BindingState::Exact {
        return Err(identity_changed("recovery file changed binding"));
    }
    if recoverable_stage {
        drop(handle);
        handle = platform::open_recoverable_stage(parent, expected, identity)?;
    }
    let proof = if prove {
        Some(prove_file(parent, expected, &handle, identity)?)
    } else {
        let (size, _) = platform::file_receipt_fields(&handle)?;
        if size > MAX_RECOVERABLE_FILE_BYTES {
            return Err(invalid("recovery file exceeds its size bound"));
        }
        None
    };
    Ok(ObservedEntry::File(ObservedFile {
        handle,
        identity,
        proof,
    }))
}

fn observe_unowned_coordinate(
    parent: &platform::DirectoryHandle,
    name: &RecoveryName,
) -> io::Result<ObservedEntry> {
    let expected = OsStr::new(name.as_str());
    let listing = platform::entries(parent, MAX_DIRECTORY_LIST_ENTRIES)?;
    if !listing.complete {
        return Err(invalid("recovery leaf enumeration exceeded its bound"));
    }
    if listing
        .entries
        .iter()
        .any(|(candidate, _)| leaf_names_equivalent(candidate, expected))
    {
        Ok(ObservedEntry::UnownedOccupied)
    } else {
        Ok(ObservedEntry::Absent)
    }
}

fn present(entry: &ObservedEntry) -> bool {
    matches!(
        entry,
        ObservedEntry::File(_) | ObservedEntry::UnownedOccupied
    )
}

fn require_proof(entry: &ObservedEntry, expected: RecoveryFileProof) -> io::Result<()> {
    match entry {
        ObservedEntry::File(file) if file.proof == Some(expected) => Ok(()),
        ObservedEntry::File(_) => Err(identity_changed("recovery content proof changed")),
        ObservedEntry::Absent | ObservedEntry::Unowned | ObservedEntry::UnownedOccupied => {
            Err(invalid("recovery payload is absent"))
        }
    }
}

fn revalidate(
    parent: &platform::DirectoryHandle,
    name: &RecoveryName,
    file: &ObservedFile,
    expected: Option<RecoveryFileProof>,
) -> io::Result<()> {
    let name = OsStr::new(name.as_str());
    if platform::file_identity(&file.handle)? != file.identity
        || platform::file_binding_state(parent, name, file.identity)? != BindingState::Exact
    {
        return Err(identity_changed("recovery payload changed binding"));
    }
    if let Some(expected) = expected
        && prove_file(parent, name, &file.handle, file.identity)? != expected
    {
        return Err(identity_changed(
            "recovery payload changed after validation",
        ));
    }
    Ok(())
}

fn plan_replay(
    root: &platform::RootGuard,
    journal: &RecoveryJournal,
) -> io::Result<Vec<ReplayPlan>> {
    let records = journal
        .records()
        .map(|(registration, record)| (registration, record.clone()))
        .collect::<Vec<_>>();
    let mut identities = HashSet::new();
    let mut coordinates: Vec<(platform::Identity, RecoveryName)> = Vec::new();
    let mut plans = Vec::with_capacity(records.len());
    for (registration, record) in records {
        if record.old.is_some() {
            return Err(invalid("replacement recovery is not active"));
        }
        let (parent, ancestors, parent_bindings) = open_parent(root, &record.destination_parent)?;
        let parent_identity = platform::directory_identity(&parent)?;
        let stage_name = recovery_stage_leaf(record.operation_id);
        let park_name = record
            .old
            .is_some()
            .then(|| recovery_park_leaf(record.operation_id));
        let mut footprint = vec![&stage_name];
        if let Some(park_name) = park_name.as_ref() {
            footprint.push(park_name);
        }
        if matches!(
            record.phase,
            RecoveryPhase::StageSealed
                | RecoveryPhase::ReplacePrepared
                | RecoveryPhase::PublishPrepared
                | RecoveryPhase::RemoveCommitted
        ) {
            footprint.push(&record.destination_leaf);
        }
        for name in footprint {
            if coordinates.iter().any(|(physical, prior)| {
                *physical == parent_identity
                    && leaf_names_equivalent(OsStr::new(prior.as_str()), OsStr::new(name.as_str()))
            }) {
                return Err(invalid("recovery records overlap one physical footprint"));
            }
            coordinates.push((parent_identity, name.clone()));
        }
        let prove_stage = !matches!(record.phase, RecoveryPhase::StagePrepared);
        let stage = observe(&parent, &stage_name, prove_stage, true)?;
        let target = match record.phase {
            RecoveryPhase::StagePrepared | RecoveryPhase::RemovePrepared => ObservedEntry::Unowned,
            RecoveryPhase::StageSealed => {
                observe_unowned_coordinate(&parent, &record.destination_leaf)?
            }
            RecoveryPhase::PublishPrepared if present(&stage) => {
                observe_unowned_coordinate(&parent, &record.destination_leaf)?
            }
            RecoveryPhase::PublishPrepared | RecoveryPhase::RemoveCommitted => {
                observe(&parent, &record.destination_leaf, true, false)?
            }
            RecoveryPhase::ReplacePrepared => return Err(codec_error()),
        };
        for entry in [&stage, &target] {
            if let ObservedEntry::File(file) = entry
                && !identities.insert(file.identity)
            {
                return Err(invalid("recovery records alias one physical file"));
            }
        }
        match record.phase {
            RecoveryPhase::StagePrepared => {}
            RecoveryPhase::StageSealed => {
                require_proof(&stage, record.new.ok_or_else(codec_error)?)?;
            }
            RecoveryPhase::PublishPrepared => match (&stage, &target) {
                (
                    ObservedEntry::File(_),
                    ObservedEntry::Absent | ObservedEntry::UnownedOccupied,
                ) => {
                    require_proof(&stage, record.new.ok_or_else(codec_error)?)?;
                }
                (ObservedEntry::Absent, ObservedEntry::File(_)) => {
                    require_proof(&target, record.new.ok_or_else(codec_error)?)?;
                }
                _ => return Err(invalid("recovery publication carrier is not linear")),
            },
            RecoveryPhase::RemovePrepared => {
                if present(&stage) {
                    require_proof(&stage, record.new.ok_or_else(codec_error)?)?;
                }
            }
            RecoveryPhase::RemoveCommitted => {
                if present(&stage) || !present(&target) {
                    return Err(invalid("committed recovery topology is invalid"));
                }
                require_proof(&target, record.new.ok_or_else(codec_error)?)?;
            }
            RecoveryPhase::ReplacePrepared => return Err(codec_error()),
        }
        plans.push(ReplayPlan {
            registration,
            record,
            parent,
            parent_identity,
            ancestors,
            parent_bindings,
            stage,
            target,
        });
    }
    Ok(plans)
}

fn remove_stage(root: &platform::RootGuard, plan: &mut ReplayPlan) -> io::Result<()> {
    let ObservedEntry::File(stage) = &plan.stage else {
        return Ok(());
    };
    let name = recovery_stage_leaf(plan.record.operation_id);
    revalidate(&plan.parent, &name, stage, None)?;
    let mut cleanup = platform::clone_stage_cleanup(
        &plan.parent,
        OsStr::new(name.as_str()),
        &stage.handle,
        stage.identity,
    )?;
    validate_parent_chain(root, plan)?;
    platform::remove_parked_file(
        &plan.parent,
        OsStr::new(name.as_str()),
        &mut cleanup,
        stage.identity,
    )?;
    validate_parent_chain(root, plan)
}

fn replay_publication(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plan: &mut ReplayPlan,
) -> io::Result<()> {
    if plan.record.phase == RecoveryPhase::StageSealed {
        plan.record.phase = RecoveryPhase::PublishPrepared;
        validate_parent_chain(root, plan)?;
        journal.advance(lease, plan.registration, plan.record.clone())?;
    }
    match (&plan.stage, &plan.target) {
        (ObservedEntry::File(stage), ObservedEntry::Absent) => {
            let stage_name = recovery_stage_leaf(plan.record.operation_id);
            revalidate(&plan.parent, &stage_name, stage, plan.record.new)?;
            let (size, stamp) = platform::file_receipt_fields(&stage.handle)?;
            let attempt =
                u64::from_le_bytes(plan.record.operation_id[..8].try_into().unwrap()).max(1);
            let mut receipt = platform::prepare_publication(
                attempt,
                &stage.handle,
                size,
                stamp,
                &plan.parent,
                OsStr::new(stage_name.as_str()),
                &plan.parent,
                OsStr::new(plan.record.destination_leaf.as_str()),
            )?;
            validate_parent_chain(root, plan)?;
            platform::rename_no_replace(
                &mut receipt,
                attempt,
                &plan.parent,
                OsStr::new(stage_name.as_str()),
                &stage.handle,
                &plan.parent,
                OsStr::new(plan.record.destination_leaf.as_str()),
            )?;
            validate_parent_chain(root, plan)?;
            platform::settle_publication(
                &mut receipt,
                attempt,
                &stage.handle,
                &plan.parent,
                OsStr::new(stage_name.as_str()),
                &plan.parent,
                OsStr::new(plan.record.destination_leaf.as_str()),
            )?;
        }
        (ObservedEntry::Absent, ObservedEntry::File(target)) => {
            revalidate(
                &plan.parent,
                &plan.record.destination_leaf,
                target,
                plan.record.new,
            )?;
            #[cfg(unix)]
            validate_parent_chain(root, plan)?;
            #[cfg(unix)]
            platform::sync_publication_directory(&plan.parent)?;
        }
        _ => return Err(invalid("recovery publication topology changed")),
    }
    validate_parent_chain(root, plan)?;
    #[cfg(unix)]
    journal.clear(lease, plan.registration)?;
    #[cfg(windows)]
    {
        plan.record.phase = RecoveryPhase::RemoveCommitted;
        journal.advance(lease, plan.registration, plan.record.clone())?;
    }
    Ok(())
}

fn replay_removal(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plan: &mut ReplayPlan,
) -> io::Result<()> {
    if plan.record.phase != RecoveryPhase::RemovePrepared {
        let mut intended = plan.record.clone();
        intended.phase = RecoveryPhase::RemovePrepared;
        validate_parent_chain(root, plan)?;
        journal.advance(lease, plan.registration, intended.clone())?;
        plan.record = intended;
    }
    remove_stage(root, plan)?;
    #[cfg(unix)]
    {
        validate_parent_chain(root, plan)?;
        platform::sync_publication_directory(&plan.parent)?;
        validate_parent_chain(root, plan)?;
        journal.clear(lease, plan.registration)?;
    }
    Ok(())
}

fn replay(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plans: &mut [ReplayPlan],
) -> io::Result<()> {
    for plan in plans {
        match plan.record.phase {
            RecoveryPhase::StagePrepared => {
                remove_stage(root, plan)?;
                #[cfg(unix)]
                {
                    validate_parent_chain(root, plan)?;
                    platform::sync_publication_directory(&plan.parent)?;
                    validate_parent_chain(root, plan)?;
                    journal.clear(lease, plan.registration)?;
                }
            }
            RecoveryPhase::StageSealed => {
                if present(&plan.target) {
                    replay_removal(root, lease, journal, plan)?;
                } else {
                    replay_publication(root, lease, journal, plan)?;
                }
            }
            RecoveryPhase::PublishPrepared => {
                if matches!(&plan.stage, ObservedEntry::File(_)) && present(&plan.target) {
                    replay_removal(root, lease, journal, plan)?;
                } else {
                    replay_publication(root, lease, journal, plan)?;
                }
            }
            RecoveryPhase::RemovePrepared => replay_removal(root, lease, journal, plan)?,
            RecoveryPhase::RemoveCommitted => {
                #[cfg(unix)]
                {
                    validate_parent_chain(root, plan)?;
                    platform::sync_publication_directory(&plan.parent)?;
                    validate_parent_chain(root, plan)?;
                    journal.clear(lease, plan.registration)?;
                }
            }
            RecoveryPhase::ReplacePrepared => return Err(codec_error()),
        }
    }
    Ok(())
}

pub(crate) fn initialize_and_replay(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
) -> io::Result<(RecoveryJournal, Vec<crate::RecoveryOrphan>)> {
    let mut journal = RecoveryJournal::load(lease)?;
    let mut plans = plan_replay(root, &journal)?;
    #[cfg(test)]
    run_replay_parent_validation_hook();
    replay(root, lease, &mut journal, &mut plans)?;
    let final_plans = plan_replay(root, &journal)?;
    let orphans = final_plans
        .into_iter()
        .filter(|plan| journal.record(plan.registration).is_some())
        .map(|plan| crate::RecoveryOrphan {
            registration: plan.registration,
            parent: plan.parent_identity,
            ancestors: plan.ancestors,
            files: [&plan.stage, &plan.target]
                .into_iter()
                .filter_map(|entry| match entry {
                    ObservedEntry::File(file) => Some(file.identity),
                    ObservedEntry::Absent
                    | ObservedEntry::Unowned
                    | ObservedEntry::UnownedOccupied => None,
                })
                .collect(),
        })
        .collect();
    Ok((journal, orphans))
}
