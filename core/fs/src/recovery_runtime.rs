use crate::platform::{self, BindingState};
use crate::recovery::{
    MAX_LIVE_PROOF_BYTES, MAX_RECOVERABLE_FILE_BYTES, RecoveryFileProof, RecoveryJournal,
    RecoveryName, RecoveryPhase, RecoveryRecord, RecoveryRegistration, ReplacementCarrier,
    classify_replacement, recovery_park_leaf, recovery_stage_leaf,
};
use crate::{
    EntryKind, LeafNameEquivalenceKey, MAX_DIRECTORY_LIST_ENTRIES, identity_changed,
    leaf_name_equivalence_keys,
};
use sha2::{Digest as _, Sha256};
use std::collections::{HashMap, HashSet};
use std::ffi::OsStr;
use std::fs::File;
use std::io;
use std::ops::{ControlFlow, Deref};
use std::sync::{Arc, OnceLock};

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
    receipt: (u64, platform::FileStamp),
    proof: Option<RecoveryFileProof>,
}

enum ObservedEntry {
    Absent,
    File(ObservedFile),
    Unowned,
    UnownedOccupied,
}

struct RetainedDirectory {
    handle: platform::DirectoryHandle,
    identity: platform::Identity,
    stamp: OnceLock<platform::DirectoryStamp>,
    parent: Option<(Arc<RetainedDirectory>, RecoveryName)>,
}

impl Deref for RetainedDirectory {
    type Target = platform::DirectoryHandle;

    fn deref(&self) -> &Self::Target {
        &self.handle
    }
}

impl RetainedDirectory {
    fn stamp(&self) -> io::Result<platform::DirectoryStamp> {
        self.stamp
            .get()
            .copied()
            .ok_or_else(|| invalid("recovery directory snapshot is incomplete"))
    }
}

struct ReplayPlan {
    registration: RecoveryRegistration,
    record: RecoveryRecord,
    parent: Arc<RetainedDirectory>,
    stage: ObservedEntry,
    target: ObservedEntry,
    park: ObservedEntry,
}

#[derive(Default)]
struct ParentNode {
    records: Vec<usize>,
    children: Vec<(RecoveryName, ParentNode)>,
}

impl ParentNode {
    fn insert(&mut self, components: &[RecoveryName], record: usize) {
        let Some((component, rest)) = components.split_first() else {
            self.records.push(record);
            return;
        };
        let index = self
            .children
            .iter()
            .position(|(name, _)| name == component)
            .unwrap_or_else(|| {
                self.children
                    .push((component.clone(), ParentNode::default()));
                self.children.len() - 1
            });
        self.children[index].1.insert(rest, record);
    }
}

struct ParentObservation {
    parent: Arc<RetainedDirectory>,
    present: [bool; 3],
}

struct ReplayAdmission {
    plans: Vec<ReplayPlan>,
    directories: Vec<Arc<RetainedDirectory>>,
    partial_files: Vec<File>,
}

#[must_use = "failed recovery admission retains every partially opened carrier"]
struct ReplayAdmissionFailure(io::Error, ReplayAdmission);

impl ReplayAdmissionFailure {
    fn acknowledge(self) -> io::Error {
        let Self(error, admission) = self;
        drop(admission);
        error
    }
}

struct ScanRequest {
    name: RecoveryName,
    kind: Option<EntryKind>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn codec_error() -> io::Error {
    invalid("root recovery control contains an invalid record")
}

fn projected_names(record: &RecoveryRecord) -> [Option<RecoveryName>; 3] {
    [
        Some(recovery_stage_leaf(record.operation_id)),
        record
            .phase
            .owns_target()
            .then(|| record.destination_leaf.clone()),
        (record.old.is_some() && record.phase.owns_park())
            .then(|| recovery_park_leaf(record.operation_id)),
    ]
}

fn unowned_occupancy(record: &RecoveryRecord, stage_present: bool) -> bool {
    record.old.is_none()
        && (record.phase == RecoveryPhase::StageSealed
            || record.phase == RecoveryPhase::PublishPrepared && stage_present)
}

fn proof_projection(plan: &ReplayPlan) -> [bool; 3] {
    let sealed = plan.record.phase != RecoveryPhase::StagePrepared;
    [
        sealed,
        matches!(plan.target, ObservedEntry::File(_)),
        sealed,
    ]
}

fn validate_parent_chain(root: &platform::RootGuard, plan: &ReplayPlan) -> io::Result<()> {
    platform::validate_root(root)?;
    let current_root = platform::clone_root(root)?;
    let root_identity = platform::directory_identity(&current_root)?;
    let mut child = &plan.parent;
    while let Some((parent, name)) = &child.parent {
        if platform::directory_identity(parent)? != parent.identity
            || platform::directory_revision(parent)? != parent.stamp()?
        {
            return Err(identity_changed("recovery parent handle changed identity"));
        }
        if platform::directory_binding_state(parent, OsStr::new(name.as_str()), child.identity)?
            != BindingState::Exact
        {
            return Err(identity_changed("recovery parent binding changed"));
        }
        if platform::directory_identity(child)? != child.identity {
            return Err(identity_changed("recovery retained parent chain changed"));
        }
        child = parent;
    }
    if child.identity != root_identity || platform::directory_identity(child)? != root_identity {
        return Err(identity_changed("recovery root handle changed"));
    }
    Ok(())
}

pub(crate) fn prove_file(
    parent: &platform::DirectoryHandle,
    name: &OsStr,
    file: &File,
    identity: platform::Identity,
) -> io::Result<RecoveryFileProof> {
    prove_file_with_receipt(
        parent,
        name,
        file,
        identity,
        platform::file_receipt_fields(file)?,
    )
}

fn prove_file_with_receipt(
    parent: &platform::DirectoryHandle,
    name: &OsStr,
    file: &File,
    identity: platform::Identity,
    expected_receipt: (u64, platform::FileStamp),
) -> io::Result<RecoveryFileProof> {
    let before = platform::file_receipt_fields(file)?;
    if expected_receipt != before {
        return Err(identity_changed(
            "recovery file changed after proof budgeting",
        ));
    }
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
        || platform::file_receipt_fields(&file.handle)? != file.receipt
        || platform::file_binding_state(parent, name, file.identity)? != BindingState::Exact
    {
        return Err(identity_changed("recovery payload changed binding"));
    }
    if let Some(expected) = expected
        && file.proof != Some(expected)
    {
        return Err(identity_changed(
            "recovery payload changed after validation",
        ));
    }
    Ok(())
}

fn retain_directory(
    admission: &mut ReplayAdmission,
    handle: platform::DirectoryHandle,
    identity: platform::Identity,
    parent: Option<(Arc<RetainedDirectory>, RecoveryName)>,
) -> Arc<RetainedDirectory> {
    let directory = Arc::new(RetainedDirectory {
        handle,
        identity,
        stamp: OnceLock::new(),
        parent,
    });
    admission.directories.push(Arc::clone(&directory));
    directory
}

fn scan_parent(
    node: &ParentNode,
    records: &[(RecoveryRegistration, RecoveryRecord)],
    directory: Arc<RetainedDirectory>,
    observations: &mut [Option<ParentObservation>],
    admission: &mut ReplayAdmission,
    physical: &mut HashSet<platform::Identity>,
) -> io::Result<()> {
    if !physical.insert(directory.identity) {
        return Err(invalid("recovery paths alias one physical directory"));
    }
    let revision = platform::directory_revision(&directory)?;
    directory
        .stamp
        .set(revision)
        .map_err(|_| invalid("recovery directory was scanned twice"))?;

    let mut requests = node
        .children
        .iter()
        .map(|(name, _)| ScanRequest {
            name: name.clone(),
            kind: None,
        })
        .collect::<Vec<_>>();
    let child_count = requests.len();
    let mut coordinates = Vec::new();
    for &record_index in &node.records {
        let record = &records[record_index].1;
        observations[record_index] = Some(ParentObservation {
            parent: Arc::clone(&directory),
            present: [false; 3],
        });
        for (coordinate, name) in projected_names(record)
            .into_iter()
            .enumerate()
            .filter_map(|(coordinate, name)| name.map(|name| (coordinate, name)))
        {
            coordinates.push((record_index, coordinate, requests.len()));
            requests.push(ScanRequest { name, kind: None });
        }
    }

    let mut lookup = HashMap::<LeafNameEquivalenceKey, usize>::new();
    for (index, request) in requests.iter().enumerate() {
        for key in leaf_name_equivalence_keys(OsStr::new(request.name.as_str())) {
            if lookup
                .insert(key, index)
                .is_some_and(|prior| prior != index)
            {
                return Err(invalid("recovery records overlap one physical footprint"));
            }
        }
    }
    let completion =
        platform::visit_entries(&directory, MAX_DIRECTORY_LIST_ENTRIES, |candidate, kind| {
            let mut matched = None;
            for key in leaf_name_equivalence_keys(candidate) {
                if let Some(&index) = lookup.get(&key)
                    && matched.replace(index).is_some_and(|prior| prior != index)
                {
                    return Err(invalid("recovery leaf matches multiple coordinates"));
                }
            }
            if let Some(index) = matched {
                let request = &mut requests[index];
                if candidate != OsStr::new(request.name.as_str())
                    || request.kind.replace(kind).is_some()
                {
                    return Err(invalid("recovery leaf has a portable alias"));
                }
            }
            Ok(ControlFlow::Continue(()))
        })?;
    if completion != platform::VisitCompletion::Complete {
        return Err(invalid("recovery directory enumeration exceeded its bound"));
    }
    if platform::directory_revision(&directory)? != revision {
        return Err(identity_changed(
            "recovery directory changed while being scanned",
        ));
    }
    if requests[..child_count]
        .iter()
        .any(|request| request.kind != Some(EntryKind::Directory))
    {
        return Err(invalid("recovery parent is absent or has the wrong type"));
    }
    for &(record, coordinate, request) in &coordinates {
        let present = requests[request].kind.is_some();
        let observation = observations[record]
            .as_mut()
            .expect("record parent was observed");
        observation.present[coordinate] = present;
        if records[record].1.old.is_some()
            && observation
                .present
                .into_iter()
                .filter(|present| *present)
                .count()
                > 2
        {
            return Err(invalid(
                "replacement recovery retains more than two physical carriers",
            ));
        }
        if requests[request]
            .kind
            .is_some_and(|kind| kind != EntryKind::File)
            && !(coordinate == 1 && records[record].1.old.is_none())
        {
            return Err(invalid("recovery coordinate has the wrong type"));
        }
    }

    for (name, child_node) in &node.children {
        let expected = OsStr::new(name.as_str());
        let (handle, identity) = platform::open_directory(&directory, expected)?;
        let child = retain_directory(
            admission,
            handle,
            identity,
            Some((Arc::clone(&directory), name.clone())),
        );
        if platform::directory_binding_state(&directory, expected, identity)? != BindingState::Exact
            || platform::directory_revision(&directory)? != revision
        {
            return Err(identity_changed(
                "recovery parent changed while being opened",
            ));
        }
        scan_parent(
            child_node,
            records,
            child,
            observations,
            admission,
            physical,
        )?;
    }
    Ok(())
}

fn open_observed(
    admission: &mut ReplayAdmission,
    parent: &Arc<RetainedDirectory>,
    name: &RecoveryName,
    present: bool,
    exclusive: bool,
    occupied: bool,
) -> io::Result<ObservedEntry> {
    if !present {
        return Ok(ObservedEntry::Absent);
    }
    if occupied {
        return Ok(ObservedEntry::UnownedOccupied);
    }
    let name = OsStr::new(name.as_str());
    admission
        .partial_files
        .push(platform::open_file(parent, name)?);
    let ordinary = admission.partial_files.len() - 1;
    let identity = platform::file_identity(&admission.partial_files[ordinary])?;
    if platform::file_binding_state(parent, name, identity)? != BindingState::Exact {
        return Err(identity_changed("recovery file changed binding"));
    }
    let selected = if exclusive {
        admission
            .partial_files
            .push(platform::open_recoverable_stage(parent, name, identity)?);
        admission.partial_files.len() - 1
    } else {
        ordinary
    };
    let receipt = platform::file_receipt_fields(&admission.partial_files[selected])?;
    if receipt.0 > MAX_RECOVERABLE_FILE_BYTES {
        return Err(invalid("recovery file exceeds its size bound"));
    }
    if platform::file_identity(&admission.partial_files[selected])? != identity
        || platform::file_binding_state(parent, name, identity)? != BindingState::Exact
    {
        return Err(identity_changed("recovery file changed while being opened"));
    }
    let file = ObservedFile {
        handle: admission.partial_files[selected].try_clone()?,
        identity,
        receipt,
        proof: None,
    };
    Ok(ObservedEntry::File(file))
}

fn validate_plan_snapshot(root: &platform::RootGuard, plan: &ReplayPlan) -> io::Result<()> {
    validate_parent_chain(root, plan)?;
    let revision = plan.parent.stamp()?;
    if platform::directory_revision(&plan.parent)? != revision {
        return Err(identity_changed("recovery destination directory changed"));
    }
    for (name, entry) in
        projected_names(&plan.record)
            .into_iter()
            .zip([&plan.stage, &plan.target, &plan.park])
    {
        if let (Some(name), ObservedEntry::File(file)) = (name, entry)
            && (platform::file_identity(&file.handle)? != file.identity
                || platform::file_receipt_fields(&file.handle)? != file.receipt
                || platform::file_binding_state(
                    &plan.parent,
                    OsStr::new(name.as_str()),
                    file.identity,
                )? != BindingState::Exact)
        {
            return Err(identity_changed("recovery carrier changed after admission"));
        }
    }
    if platform::directory_revision(&plan.parent)? != revision {
        return Err(identity_changed(
            "recovery directory changed during validation",
        ));
    }
    validate_parent_chain(root, plan)
}

fn carrier(entry: &ObservedEntry, unsealed: bool, record: &RecoveryRecord) -> ReplacementCarrier {
    match entry {
        ObservedEntry::Unowned => ReplacementCarrier::Unobserved,
        ObservedEntry::Absent => ReplacementCarrier::Absent,
        ObservedEntry::UnownedOccupied => ReplacementCarrier::Other,
        ObservedEntry::File(_) if unsealed => ReplacementCarrier::Unsealed,
        ObservedEntry::File(file) => match (
            record.old.is_some_and(|proof| file.proof == Some(proof)),
            record.new.is_some_and(|proof| file.proof == Some(proof)),
        ) {
            (true, true) => ReplacementCarrier::OldAndNew,
            (true, false) => ReplacementCarrier::Old,
            (false, true) => ReplacementCarrier::New,
            (false, false) => ReplacementCarrier::Other,
        },
    }
}

fn plan_replay(
    root: &platform::RootGuard,
    journal: &RecoveryJournal,
) -> Result<Vec<ReplayPlan>, ReplayAdmissionFailure> {
    let records = journal
        .records()
        .map(|(registration, record)| (registration, record.clone()))
        .collect::<Vec<_>>();
    let mut tree = ParentNode::default();
    for (index, (_, record)) in records.iter().enumerate() {
        tree.insert(&record.destination_parent, index);
    }
    let mut admission = ReplayAdmission {
        plans: Vec::with_capacity(records.len()),
        directories: Vec::new(),
        partial_files: Vec::new(),
    };
    let result = (|| -> io::Result<()> {
        let root_handle = platform::clone_root(root)?;
        let root_identity = platform::directory_identity(&root_handle)?;
        let root_directory = retain_directory(&mut admission, root_handle, root_identity, None);
        let mut observations = (0..records.len()).map(|_| None).collect::<Vec<_>>();
        scan_parent(
            &tree,
            &records,
            root_directory,
            &mut observations,
            &mut admission,
            &mut HashSet::new(),
        )?;
        for (index, (registration, record)) in records.into_iter().enumerate() {
            let observation = observations[index]
                .take()
                .expect("recovery record parent was observed");
            let [Some(stage_name), target_name, park_name] = projected_names(&record) else {
                unreachable!("recovery stage is always projected")
            };
            let stage = open_observed(
                &mut admission,
                &observation.parent,
                &stage_name,
                observation.present[0],
                true,
                false,
            )?;
            let target_occupied = unowned_occupancy(&record, present(&stage));
            let target = if let Some(target_name) = &target_name {
                open_observed(
                    &mut admission,
                    &observation.parent,
                    target_name,
                    observation.present[1],
                    record.old.is_some(),
                    target_occupied,
                )?
            } else {
                ObservedEntry::Unowned
            };
            let park = if let Some(park_name) = &park_name {
                open_observed(
                    &mut admission,
                    &observation.parent,
                    park_name,
                    observation.present[2],
                    true,
                    false,
                )?
            } else {
                ObservedEntry::Unowned
            };
            admission.plans.push(ReplayPlan {
                registration,
                record,
                parent: observation.parent,
                stage,
                target,
                park,
            });
        }

        let mut identities = HashSet::new();
        let mut proof_bytes = 0_u64;
        for plan in &admission.plans {
            let proof_required = proof_projection(plan);
            for (coordinate, entry) in [&plan.stage, &plan.target, &plan.park]
                .into_iter()
                .enumerate()
            {
                let ObservedEntry::File(file) = entry else {
                    continue;
                };
                if !identities.insert(file.identity) {
                    return Err(invalid("recovery records alias one physical file"));
                }
                if proof_required[coordinate] {
                    proof_bytes = proof_bytes
                        .checked_add(file.receipt.0)
                        .ok_or_else(|| invalid("recovery proof budget overflowed"))?;
                }
            }
        }
        if proof_bytes > MAX_LIVE_PROOF_BYTES {
            return Err(invalid("recovery proof lane exceeds its byte bound"));
        }
        for plan in &admission.plans {
            validate_plan_snapshot(root, plan)?;
        }
        for plan in &mut admission.plans {
            let parent = Arc::clone(&plan.parent);
            let stage_name = recovery_stage_leaf(plan.record.operation_id);
            let park_name = recovery_park_leaf(plan.record.operation_id);
            let proof_required = proof_projection(plan);
            for (coordinate, (name, entry)) in [
                (&stage_name, &mut plan.stage),
                (&plan.record.destination_leaf, &mut plan.target),
                (&park_name, &mut plan.park),
            ]
            .into_iter()
            .enumerate()
            {
                if proof_required[coordinate]
                    && let ObservedEntry::File(file) = entry
                {
                    file.proof = Some(prove_file_with_receipt(
                        &parent,
                        OsStr::new(name.as_str()),
                        &file.handle,
                        file.identity,
                        file.receipt,
                    )?);
                }
            }
        }
        for plan in &admission.plans {
            validate_plan_snapshot(root, plan)?;
            if plan.record.old.is_some() {
                classify_replacement(
                    &plan.record,
                    (
                        carrier(
                            &plan.stage,
                            plan.record.phase == RecoveryPhase::StagePrepared,
                            &plan.record,
                        ),
                        carrier(&plan.target, false, &plan.record),
                        carrier(&plan.park, false, &plan.record),
                    ),
                )
                .ok_or_else(|| invalid("replacement recovery topology is invalid"))?;
                continue;
            }
            match plan.record.phase {
                RecoveryPhase::StagePrepared => {}
                RecoveryPhase::StageSealed => {
                    require_proof(&plan.stage, plan.record.new.ok_or_else(codec_error)?)?;
                }
                RecoveryPhase::PublishPrepared => match (&plan.stage, &plan.target) {
                    (
                        ObservedEntry::File(_),
                        ObservedEntry::Absent | ObservedEntry::UnownedOccupied,
                    ) => require_proof(&plan.stage, plan.record.new.ok_or_else(codec_error)?)?,
                    (ObservedEntry::Absent, ObservedEntry::File(_)) => {
                        require_proof(&plan.target, plan.record.new.ok_or_else(codec_error)?)?
                    }
                    _ => return Err(invalid("recovery publication carrier is not linear")),
                },
                RecoveryPhase::RemovePrepared => {
                    if present(&plan.stage) {
                        require_proof(&plan.stage, plan.record.new.ok_or_else(codec_error)?)?;
                    }
                }
                RecoveryPhase::RemoveCommitted => {
                    if present(&plan.stage) || !present(&plan.target) {
                        return Err(invalid("committed recovery topology is invalid"));
                    }
                    require_proof(&plan.target, plan.record.new.ok_or_else(codec_error)?)?;
                }
                RecoveryPhase::ReplacePrepared => return Err(codec_error()),
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(std::mem::take(&mut admission.plans)),
        Err(error) => Err(ReplayAdmissionFailure(error, admission)),
    }
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
        journal.advance(lease, plan.registration, intended)?;
        plan.record.phase = RecoveryPhase::RemovePrepared;
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
    if plans.iter().any(|plan| plan.record.old.is_some()) {
        return Err(invalid("replacement recovery effects are not active"));
    }
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
    let mut plans = plan_replay(root, &journal).map_err(ReplayAdmissionFailure::acknowledge)?;
    #[cfg(test)]
    run_replay_parent_validation_hook();
    replay(root, lease, &mut journal, &mut plans)?;
    let final_plans = plan_replay(root, &journal).map_err(ReplayAdmissionFailure::acknowledge)?;
    let orphans = final_plans
        .into_iter()
        .map(|plan| {
            let mut ancestors = Vec::new();
            let mut current = Some(&plan.parent);
            while let Some(directory) = current {
                ancestors.push(directory.identity);
                current = directory.parent.as_ref().map(|(parent, _)| parent);
            }
            ancestors.reverse();
            crate::RecoveryOrphan {
                registration: plan.registration,
                ancestors,
                files: [&plan.stage, &plan.target, &plan.park]
                    .into_iter()
                    .filter_map(|entry| match entry {
                        ObservedEntry::File(file) => Some(file.identity),
                        _ => None,
                    })
                    .collect(),
            }
        })
        .collect();
    Ok((journal, orphans))
}

#[cfg(test)]
mod admission_tests {
    use super::*;

    fn name(value: &str) -> RecoveryName {
        RecoveryName::new_exact(value).expect("test recovery name")
    }

    fn record(phase: RecoveryPhase, replacing: bool) -> RecoveryRecord {
        let proof = RecoveryFileProof {
            size: 1,
            sha256: [0x41; 32],
        };
        RecoveryRecord {
            operation_id: [0x31; 16],
            phase,
            destination_parent: Vec::new(),
            destination_leaf: name("target.bin"),
            old: replacing.then_some(proof),
            new: (phase != RecoveryPhase::StagePrepared).then_some(proof),
        }
    }

    #[test]
    fn create_only_prepublication_target_is_unowned_occupancy() {
        assert!(unowned_occupancy(
            &record(RecoveryPhase::StageSealed, false),
            false,
        ));
        assert!(unowned_occupancy(
            &record(RecoveryPhase::PublishPrepared, false),
            true,
        ));
        assert!(!unowned_occupancy(
            &record(RecoveryPhase::PublishPrepared, false),
            false,
        ));
        assert!(!unowned_occupancy(
            &record(RecoveryPhase::StageSealed, true),
            true,
        ));
    }

    #[test]
    fn parent_trie_groups_shared_ancestry_before_io() {
        let mut tree = ParentNode::default();
        tree.insert(&[name("shared"), name("left")], 0);
        tree.insert(&[name("shared"), name("right")], 1);
        tree.insert(&[name("shared"), name("left")], 2);

        assert_eq!(tree.children.len(), 1);
        let shared = &tree.children[0].1;
        assert_eq!(shared.children.len(), 2);
        assert_eq!(shared.children[0].1.records, [0, 2]);
        assert_eq!(shared.children[1].1.records, [1]);
    }
}
