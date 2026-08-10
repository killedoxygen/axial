use crate::platform::{self, BindingState};
use crate::recovery::{
    MAX_LIVE_PROOF_BYTES, MAX_RECOVERABLE_FILE_BYTES, RecoveryFileProof, RecoveryJournal,
    RecoveryName, RecoveryPhase, RecoveryRecord, RecoveryRegistration, ReplacementAction,
    ReplacementCarrier, StateSuccessorDescriptor, SuccessorOwner, classify_replacement,
    recovery_park_leaf, recovery_stage_leaf,
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
use std::sync::{Arc, RwLock};

#[cfg(test)]
thread_local! {
    static REPLAY_PARENT_VALIDATION_HOOK: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        std::cell::RefCell::new(None);
    static REPLAY_EXCLUSIVE_ADMISSION_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REPLAY_TRANSFER_FAILURE_AFTER: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REPLAY_TRANSFER_COMMITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static REPLAY_REMOVAL_SETTLEMENT_FAILURE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static REPLAY_PENDING_REMOVAL_SETTLEMENTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    #[cfg(unix)]
    static REPLAY_TOMBSTONE_PRUNES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
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

#[cfg(test)]
pub(crate) fn fail_next_replay_exclusive_admission() {
    REPLAY_EXCLUSIVE_ADMISSION_FAILURE.with(|failure| {
        assert!(!failure.replace(true));
    });
}

#[cfg(test)]
fn take_replay_exclusive_admission_failure() -> bool {
    REPLAY_EXCLUSIVE_ADMISSION_FAILURE.with(std::cell::Cell::take)
}

#[cfg(test)]
pub(crate) fn fail_replay_transfer_after(validations: usize) {
    assert!(validations != 0);
    REPLAY_TRANSFER_FAILURE_AFTER.with(|remaining| assert_eq!(remaining.replace(validations), 0));
}

#[cfg(test)]
fn take_replay_transfer_failure() -> bool {
    REPLAY_TRANSFER_FAILURE_AFTER.with(|remaining| {
        let value = remaining.get();
        if value == 0 {
            return false;
        }
        remaining.set(value - 1);
        value == 1
    })
}

#[cfg(test)]
pub(crate) fn take_replay_transfer_commits() -> usize {
    REPLAY_TRANSFER_COMMITS.with(std::cell::Cell::take)
}

#[cfg(test)]
pub(crate) fn fail_next_replay_removal_settlement() {
    REPLAY_REMOVAL_SETTLEMENT_FAILURE.with(|failure| assert!(!failure.replace(true)));
}

#[cfg(test)]
fn take_replay_removal_settlement_failure() -> bool {
    REPLAY_REMOVAL_SETTLEMENT_FAILURE.with(std::cell::Cell::take)
}

#[cfg(test)]
pub(crate) fn take_replay_pending_removal_settlements() -> usize {
    REPLAY_PENDING_REMOVAL_SETTLEMENTS.with(std::cell::Cell::take)
}

#[cfg(all(test, unix))]
pub(crate) fn take_replay_tombstone_prunes() -> usize {
    REPLAY_TOMBSTONE_PRUNES.with(std::cell::Cell::take)
}

struct ObservedFile {
    handle: File,
    identity: platform::Identity,
    receipt: (u64, platform::FileStamp),
    proof: Option<RecoveryFileProof>,
    exclusive: bool,
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
    stamp: RwLock<Option<platform::DirectoryStamp>>,
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
            .read()
            .map_err(|_| io::Error::other("recovery directory snapshot lock was poisoned"))?
            .as_ref()
            .copied()
            .ok_or_else(|| invalid("recovery directory snapshot is incomplete"))
    }

    fn initialize_stamp(&self, stamp: platform::DirectoryStamp) -> io::Result<()> {
        let mut retained = self
            .stamp
            .write()
            .map_err(|_| io::Error::other("recovery directory snapshot lock was poisoned"))?;
        if retained.replace(stamp).is_some() {
            return Err(invalid("recovery directory was scanned twice"));
        }
        Ok(())
    }

    fn update_stamp(&self, stamp: platform::DirectoryStamp) -> io::Result<()> {
        *self
            .stamp
            .write()
            .map_err(|_| io::Error::other("recovery directory snapshot lock was poisoned"))? =
            Some(stamp);
        Ok(())
    }
}

struct ReplayPlan {
    registration: RecoveryRegistration,
    record: RecoveryRecord,
    parent: Arc<RetainedDirectory>,
    stage: ObservedEntry,
    target: ObservedEntry,
    park: ObservedEntry,
    publication: Option<platform::PublicationReceipt>,
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReplayCoordinate {
    Stage,
    Target,
    Park,
}

const REPLAY_COORDINATES: [ReplayCoordinate; 3] = [
    ReplayCoordinate::Stage,
    ReplayCoordinate::Target,
    ReplayCoordinate::Park,
];

struct RetainedCarrier {
    registration: RecoveryRegistration,
    coordinate: ReplayCoordinate,
    file: Option<ObservedFile>,
}

struct PendingRemoval {
    registration: RecoveryRegistration,
    coordinate: ReplayCoordinate,
    file: ObservedFile,
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

#[derive(Default)]
struct ReplayAdmission {
    plans: Vec<ReplayPlan>,
    directories: Vec<Arc<RetainedDirectory>>,
    partial_carriers: Vec<RetainedCarrier>,
    pending_removals: Vec<PendingRemoval>,
    retired: Vec<ObservedFile>,
}

#[must_use = "failed recovery admission retains every partially opened carrier"]
struct ReplayAdmissionFailure(io::Error, ReplayAdmission);

pub(crate) struct RecoveryReplay {
    inner: Option<Box<RecoveryReplayInner>>,
}

struct RecoveryReplayInner {
    journal: RecoveryJournal,
    state: Option<ReplayState>,
    planning_attempts: u8,
}

const MAX_REPLAY_PLANNING_ATTEMPTS: u8 = 4;

fn admit_replay_planning_attempt(attempts: &mut u8) -> io::Result<()> {
    if *attempts == MAX_REPLAY_PLANNING_ATTEMPTS {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "root recovery planning retry bound was exhausted",
        ));
    }
    *attempts += 1;
    Ok(())
}

enum ReplayState {
    Admit(ReplayRetention),
    Replan {
        retained: ReplayAdmission,
        partial: ReplayRetention,
    },
    Successor {
        owner: Option<SuccessorOwner>,
        records: Vec<(RecoveryRegistration, RecoveryRecord)>,
        retained: ReplayAdmission,
        partial: ReplayRetention,
        effects_complete: bool,
    },
}

fn retained_replay_state(
    had_prior: bool,
    retained: ReplayAdmission,
    partial: ReplayRetention,
) -> ReplayState {
    if had_prior {
        ReplayState::Replan { retained, partial }
    } else {
        ReplayState::Admit(partial)
    }
}

#[derive(Default)]
struct ReplayRetention {
    directories: Vec<Arc<RetainedDirectory>>,
    carriers: Vec<RetainedCarrier>,
    retired: Vec<ObservedFile>,
}

impl ReplayRetention {
    fn absorb(&mut self, mut admission: ReplayAdmission) {
        debug_assert!(admission.pending_removals.is_empty());
        self.directories.append(&mut admission.directories);
        self.carriers.append(&mut admission.partial_carriers);
        self.retired.append(&mut admission.retired);
        for mut plan in admission.plans {
            debug_assert!(plan.publication.is_none());
            let registration = plan.registration;
            for (coordinate, entry) in REPLAY_COORDINATES.into_iter().zip(entries_mut(&mut plan)) {
                if let ObservedEntry::File(file) = std::mem::replace(entry, ObservedEntry::Unowned)
                {
                    self.carriers.push(RetainedCarrier {
                        registration,
                        coordinate,
                        file: Some(file),
                    });
                }
            }
        }
    }
}

fn align_retained(journal: &RecoveryJournal, state: &mut ReplayState) {
    let live = |registration: RecoveryRegistration| journal.record(registration).is_some();
    match state {
        ReplayState::Admit(partial) => {
            partial
                .carriers
                .retain(|carrier| live(carrier.registration));
        }
        ReplayState::Replan { retained, partial } => {
            retained.plans.retain(|plan| {
                let retain = live(plan.registration);
                #[cfg(all(test, unix))]
                if !retain
                    && entries(plan)
                        .into_iter()
                        .any(|entry| matches!(entry, ObservedEntry::File(file) if file.exclusive))
                {
                    REPLAY_TOMBSTONE_PRUNES.with(|count| count.set(count.get() + 1));
                }
                retain
            });
            partial
                .carriers
                .retain(|carrier| live(carrier.registration));
        }
        ReplayState::Successor { .. } => {}
    }
}

fn settle_pending_removal(
    root: &platform::RootGuard,
    plans: &[ReplayPlan],
    pending: &PendingRemoval,
) -> io::Result<()> {
    let Some((index, plan)) = exactly_one(
        plans
            .iter()
            .enumerate()
            .filter(|(_, plan)| plan.registration == pending.registration),
    ) else {
        return Err(invalid("pending recovery removal lost its unique plan"));
    };
    if !matches!(entry(plan, pending.coordinate), ObservedEntry::Absent) {
        return Err(invalid("pending recovery removal changed topology"));
    }
    let name = coordinate_name(plan, pending.coordinate);
    refresh_parent_after_effect(root, plans, &plan.parent)?;
    platform::settle_removed_recoverable_stage(
        &plan.parent,
        OsStr::new(name.as_str()),
        &pending.file.handle,
        pending.file.identity,
    )?;
    validate_plan_snapshot(root, &plans[index])
}

fn settle_pending_removals(
    root: &platform::RootGuard,
    plans: &[ReplayPlan],
    pending: &mut Vec<PendingRemoval>,
    retired: &mut Vec<ObservedFile>,
) -> io::Result<()> {
    while let Some(removal) = pending.first() {
        settle_pending_removal(root, plans, removal)?;
        let removal = pending.remove(0);
        retired.push(removal.file);
    }
    Ok(())
}

fn settle_replay_state_removals(
    root: &platform::RootGuard,
    state: &mut ReplayState,
) -> io::Result<()> {
    match state {
        ReplayState::Admit(_) => Ok(()),
        ReplayState::Replan { retained, .. } => {
            #[cfg(test)]
            REPLAY_PENDING_REMOVAL_SETTLEMENTS.with(|count| {
                count.set(count.get() + retained.pending_removals.len());
            });
            settle_pending_removals(
                root,
                &retained.plans,
                &mut retained.pending_removals,
                &mut retained.retired,
            )
        }
        ReplayState::Successor { retained, .. } => settle_pending_removals(
            root,
            &retained.plans,
            &mut retained.pending_removals,
            &mut retained.retired,
        ),
    }
}

struct ScanRequest {
    name: RecoveryName,
    kind: Option<EntryKind>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn exactly_one<T>(mut items: impl Iterator<Item = T>) -> Option<T> {
    let item = items.next()?;
    items.next().is_none().then_some(item)
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
    validate_retained_parent_chain(root, &plan.parent)
}

fn validate_retained_parent_chain(
    root: &platform::RootGuard,
    parent: &Arc<RetainedDirectory>,
) -> io::Result<()> {
    platform::validate_root(root)?;
    let current_root = platform::clone_root(root)?;
    let root_identity = platform::directory_identity(&current_root)?;
    let mut child = parent;
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
        stamp: RwLock::new(None),
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
    directory.initialize_stamp(revision)?;

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

#[expect(
    clippy::too_many_arguments,
    reason = "carrier admission binds its exact recovery coordinate and retained authority in one transition"
)]
fn open_observed(
    admission: &mut ReplayAdmission,
    registration: RecoveryRegistration,
    coordinate: ReplayCoordinate,
    parent: &Arc<RetainedDirectory>,
    name: &RecoveryName,
    present: bool,
    exclusive: bool,
    retained_exclusive: &HashSet<platform::Identity>,
    retained_coordinates: &[(RecoveryRegistration, ReplayCoordinate)],
    occupied: bool,
) -> io::Result<ObservedEntry> {
    if !present {
        return Ok(ObservedEntry::Absent);
    }
    if occupied {
        return Ok(ObservedEntry::UnownedOccupied);
    }
    let name = OsStr::new(name.as_str());
    let ordinary = platform::open_file(parent, name)?;
    let identity = platform::file_identity(&ordinary)?;
    if platform::file_binding_state(parent, name, identity)? != BindingState::Exact {
        return Err(identity_changed("recovery file changed binding"));
    }
    let receipt = platform::file_receipt_fields(&ordinary)?;
    if receipt.0 > MAX_RECOVERABLE_FILE_BYTES {
        return Err(invalid("recovery file exceeds its size bound"));
    }
    let reuse_exclusive = exclusive
        && (retained_exclusive.contains(&identity)
            || retained_coordinates.contains(&(registration, coordinate)));
    if exclusive && !reuse_exclusive {
        admission.partial_carriers.push(RetainedCarrier {
            registration,
            coordinate,
            file: Some(ObservedFile {
                handle: ordinary,
                identity,
                receipt,
                proof: None,
                exclusive: false,
            }),
        });
        let selected = platform::open_recoverable_stage(parent, name, identity)?;
        admission.partial_carriers.push(RetainedCarrier {
            registration,
            coordinate,
            file: Some(ObservedFile {
                handle: selected,
                identity,
                receipt,
                proof: None,
                exclusive: true,
            }),
        });
        #[cfg(test)]
        if take_replay_exclusive_admission_failure() {
            return Err(io::Error::other(
                "injected exclusive recovery admission failure",
            ));
        }
        let retained_file = admission
            .partial_carriers
            .last_mut()
            .and_then(|carrier| carrier.file.as_mut())
            .expect("retained recovery carrier remains present");
        let selected_identity = platform::file_identity(&retained_file.handle)?;
        let selected_receipt = platform::file_receipt_fields(&retained_file.handle)?;
        retained_file.identity = selected_identity;
        retained_file.receipt = selected_receipt;
        if selected_identity != identity
            || selected_receipt != receipt
            || platform::file_binding_state(parent, name, identity)? != BindingState::Exact
        {
            return Err(identity_changed(
                "recovery file changed while acquiring exclusive authority",
            ));
        }
        let selected = admission
            .partial_carriers
            .pop()
            .and_then(|carrier| carrier.file)
            .expect("validated recovery carrier remains retained");
        admission
            .partial_carriers
            .pop()
            .expect("exclusive recovery admission retains its ordinary observation");
        return Ok(ObservedEntry::File(selected));
    }
    Ok(ObservedEntry::File(ObservedFile {
        handle: ordinary,
        identity,
        receipt,
        proof: None,
        exclusive: false,
    }))
}

fn validate_plan_snapshot(root: &platform::RootGuard, plan: &ReplayPlan) -> io::Result<()> {
    validate_parent_chain(root, plan)?;
    let revision = plan.parent.stamp()?;
    if platform::directory_revision(&plan.parent)? != revision {
        return Err(identity_changed("recovery destination directory changed"));
    }
    for (name, entry) in projected_names(&plan.record).into_iter().zip(entries(plan)) {
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

fn refresh_parent_after_effect(
    root: &platform::RootGuard,
    plans: &[ReplayPlan],
    parent: &Arc<RetainedDirectory>,
) -> io::Result<()> {
    #[derive(Clone, Copy)]
    enum Expected<'a> {
        Coordinate(&'a ObservedEntry),
        Child(platform::Identity),
    }

    struct Coordinate<'a> {
        name: RecoveryName,
        expected: Expected<'a>,
        seen: bool,
    }

    let revision = platform::directory_revision(parent)?;
    let mut coordinates = Vec::new();
    for plan in plans {
        if Arc::ptr_eq(&plan.parent, parent) {
            for (name, entry) in projected_names(&plan.record).into_iter().zip(entries(plan)) {
                if let Some(name) = name {
                    coordinates.push(Coordinate {
                        name,
                        expected: Expected::Coordinate(entry),
                        seen: false,
                    });
                }
            }
        }
        let mut child = &plan.parent;
        while let Some((ancestor, name)) = &child.parent {
            if Arc::ptr_eq(ancestor, parent)
                && !coordinates.iter().any(|coordinate| {
                    &coordinate.name == name
                        && matches!(coordinate.expected, Expected::Child(identity) if identity == child.identity)
                })
            {
                coordinates.push(Coordinate {
                    name: name.clone(),
                    expected: Expected::Child(child.identity),
                    seen: false,
                });
            }
            child = ancestor;
        }
    }
    let mut lookup = HashMap::<LeafNameEquivalenceKey, usize>::new();
    for (index, coordinate) in coordinates.iter().enumerate() {
        for key in leaf_name_equivalence_keys(OsStr::new(coordinate.name.as_str())) {
            if lookup
                .insert(key, index)
                .is_some_and(|prior| prior != index)
            {
                return Err(invalid("recovery effect coordinates overlap"));
            }
        }
    }
    let completion = platform::visit_entries(parent, MAX_DIRECTORY_LIST_ENTRIES, |name, kind| {
        let mut matched = None;
        for key in leaf_name_equivalence_keys(name) {
            if let Some(&index) = lookup.get(&key)
                && matched.replace(index).is_some_and(|prior| prior != index)
            {
                return Err(invalid("recovery effect leaf matches multiple coordinates"));
            }
        }
        if let Some(index) = matched {
            let coordinate = &mut coordinates[index];
            if name != OsStr::new(coordinate.name.as_str())
                || coordinate.seen
                || matches!(
                    coordinate.expected,
                    Expected::Coordinate(ObservedEntry::File(_))
                ) && kind != EntryKind::File
                || matches!(coordinate.expected, Expected::Child(_)) && kind != EntryKind::Directory
            {
                return Err(invalid("recovery effect changed a portable name class"));
            }
            coordinate.seen = true;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if completion != platform::VisitCompletion::Complete {
        return Err(invalid("recovery effect parent exceeds its entry bound"));
    }
    for coordinate in coordinates {
        match coordinate.expected {
            Expected::Coordinate(ObservedEntry::File(file)) if coordinate.seen => {
                revalidate(parent, &coordinate.name, file, None)?;
            }
            Expected::Coordinate(ObservedEntry::Absent) if !coordinate.seen => {}
            Expected::Coordinate(ObservedEntry::UnownedOccupied) if coordinate.seen => {}
            Expected::Child(identity)
                if coordinate.seen
                    && platform::directory_binding_state(
                        parent,
                        OsStr::new(coordinate.name.as_str()),
                        identity,
                    )? == BindingState::Exact => {}
            _ => {
                return Err(invalid(
                    "recovery effect topology changed during validation",
                ));
            }
        }
    }
    if platform::directory_revision(parent)? != revision {
        return Err(identity_changed(
            "recovery effect parent changed during validation",
        ));
    }
    parent.update_stamp(revision)?;
    plans
        .iter()
        .filter(|plan| {
            let mut current = Some(&plan.parent);
            while let Some(directory) = current {
                if Arc::ptr_eq(directory, parent) {
                    return true;
                }
                current = directory.parent.as_ref().map(|(ancestor, _)| ancestor);
            }
            false
        })
        .try_for_each(|plan| validate_parent_chain(root, plan))
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

#[expect(
    clippy::result_large_err,
    reason = "cold admission errors transfer the complete linear recovery authority"
)]
fn plan_replay(
    root: &platform::RootGuard,
    journal: &RecoveryJournal,
    retained_exclusive: &HashSet<platform::Identity>,
    retained_coordinates: &[(RecoveryRegistration, ReplayCoordinate)],
) -> Result<ReplayAdmission, ReplayAdmissionFailure> {
    let records = journal
        .records()
        .map(|(registration, record)| (registration, record.clone()))
        .collect::<Vec<_>>();
    plan_replay_records(root, records, retained_exclusive, retained_coordinates)
}

#[expect(
    clippy::result_large_err,
    reason = "cold admission errors transfer the complete linear recovery authority"
)]
fn plan_replay_records(
    root: &platform::RootGuard,
    records: Vec<(RecoveryRegistration, RecoveryRecord)>,
    retained_exclusive: &HashSet<platform::Identity>,
    retained_coordinates: &[(RecoveryRegistration, ReplayCoordinate)],
) -> Result<ReplayAdmission, ReplayAdmissionFailure> {
    let mut tree = ParentNode::default();
    for (index, (_, record)) in records.iter().enumerate() {
        tree.insert(&record.destination_parent, index);
    }
    let mut admission = ReplayAdmission {
        plans: Vec::with_capacity(records.len()),
        ..ReplayAdmission::default()
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
            admission.plans.push(ReplayPlan {
                registration,
                record,
                parent: observation.parent,
                stage: ObservedEntry::Absent,
                target: if target_name.is_some() {
                    ObservedEntry::Absent
                } else {
                    ObservedEntry::Unowned
                },
                park: if park_name.is_some() {
                    ObservedEntry::Absent
                } else {
                    ObservedEntry::Unowned
                },
                publication: None,
            });
            let plan = admission.plans.len() - 1;
            let parent = Arc::clone(&admission.plans[plan].parent);
            admission.plans[plan].stage = open_observed(
                &mut admission,
                registration,
                ReplayCoordinate::Stage,
                &parent,
                &stage_name,
                observation.present[0],
                true,
                retained_exclusive,
                retained_coordinates,
                false,
            )?;
            let target_occupied = unowned_occupancy(
                &admission.plans[plan].record,
                present(&admission.plans[plan].stage),
            );
            if let Some(target_name) = &target_name {
                let parent = Arc::clone(&admission.plans[plan].parent);
                let record = &admission.plans[plan].record;
                let exclusive = record.old.is_some()
                    || record.phase == RecoveryPhase::RemoveCommitted
                    || record.phase == RecoveryPhase::PublishPrepared
                        && !present(&admission.plans[plan].stage);
                let target = open_observed(
                    &mut admission,
                    registration,
                    ReplayCoordinate::Target,
                    &parent,
                    target_name,
                    observation.present[1],
                    exclusive,
                    retained_exclusive,
                    retained_coordinates,
                    target_occupied,
                )?;
                admission.plans[plan].target = target;
            }
            if let Some(park_name) = &park_name {
                let parent = Arc::clone(&admission.plans[plan].parent);
                let park = open_observed(
                    &mut admission,
                    registration,
                    ReplayCoordinate::Park,
                    &parent,
                    park_name,
                    observation.present[2],
                    true,
                    retained_exclusive,
                    retained_coordinates,
                    false,
                )?;
                admission.plans[plan].park = park;
            }
        }

        let mut identities = HashSet::new();
        let mut proof_bytes = 0_u64;
        for plan in &admission.plans {
            let proof_required = proof_projection(plan);
            for (coordinate, entry) in entries(plan).into_iter().enumerate() {
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
                RecoveryPhase::RemoveCommitted => match (&plan.stage, &plan.target) {
                    (ObservedEntry::File(_), ObservedEntry::Absent) => {
                        require_proof(&plan.stage, plan.record.new.ok_or_else(codec_error)?)?
                    }
                    (ObservedEntry::Absent, ObservedEntry::File(_)) => {
                        require_proof(&plan.target, plan.record.new.ok_or_else(codec_error)?)?
                    }
                    _ => return Err(invalid("committed recovery topology is invalid")),
                },
                RecoveryPhase::ReplacePrepared => return Err(codec_error()),
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => Ok(admission),
        Err(error) => Err(ReplayAdmissionFailure(error, admission)),
    }
}

fn entry(plan: &ReplayPlan, coordinate: ReplayCoordinate) -> &ObservedEntry {
    match coordinate {
        ReplayCoordinate::Stage => &plan.stage,
        ReplayCoordinate::Target => &plan.target,
        ReplayCoordinate::Park => &plan.park,
    }
}

fn entries(plan: &ReplayPlan) -> [&ObservedEntry; 3] {
    [&plan.stage, &plan.target, &plan.park]
}

fn entry_mut(plan: &mut ReplayPlan, coordinate: ReplayCoordinate) -> &mut ObservedEntry {
    match coordinate {
        ReplayCoordinate::Stage => &mut plan.stage,
        ReplayCoordinate::Target => &mut plan.target,
        ReplayCoordinate::Park => &mut plan.park,
    }
}

fn entries_mut(plan: &mut ReplayPlan) -> [&mut ObservedEntry; 3] {
    [&mut plan.stage, &mut plan.target, &mut plan.park]
}

fn coordinate_name(plan: &ReplayPlan, coordinate: ReplayCoordinate) -> RecoveryName {
    match coordinate {
        ReplayCoordinate::Stage => recovery_stage_leaf(plan.record.operation_id),
        ReplayCoordinate::Target => plan.record.destination_leaf.clone(),
        ReplayCoordinate::Park => recovery_park_leaf(plan.record.operation_id),
    }
}

fn coordinate_requires_exclusive(plan: &ReplayPlan, coordinate: ReplayCoordinate) -> bool {
    match coordinate {
        ReplayCoordinate::Stage | ReplayCoordinate::Park => true,
        ReplayCoordinate::Target => {
            plan.record.old.is_some()
                || plan.record.phase == RecoveryPhase::RemoveCommitted
                || plan.record.phase == RecoveryPhase::PublishPrepared
                    && matches!(plan.stage, ObservedEntry::Absent)
        }
    }
}

fn publication_attempt(record: &RecoveryRecord) -> u64 {
    u64::from_le_bytes(record.operation_id[..8].try_into().unwrap()).max(1)
}

fn refresh_exclusive_file(file: &mut ObservedFile) -> io::Result<()> {
    if file.exclusive {
        file.identity = platform::file_identity(&file.handle)?;
        file.receipt = platform::file_receipt_fields(&file.handle)?;
    }
    Ok(())
}

fn refresh_retained_exclusive(partial: &mut ReplayRetention) -> io::Result<()> {
    partial
        .carriers
        .iter_mut()
        .filter_map(|carrier| carrier.file.as_mut())
        .try_for_each(refresh_exclusive_file)
}

fn refresh_plan_exclusive(admission: &mut ReplayAdmission) -> io::Result<()> {
    admission
        .plans
        .iter_mut()
        .flat_map(entries_mut)
        .filter_map(|entry| match entry {
            ObservedEntry::File(file) => Some(file),
            _ => None,
        })
        .try_for_each(refresh_exclusive_file)
}

fn refresh_replay_state_exclusive(state: &mut ReplayState) -> io::Result<()> {
    match state {
        ReplayState::Admit(partial) => refresh_retained_exclusive(partial),
        ReplayState::Replan { retained, partial } => {
            refresh_plan_exclusive(retained)?;
            refresh_retained_exclusive(partial)
        }
        ReplayState::Successor {
            retained, partial, ..
        } => {
            refresh_plan_exclusive(retained)?;
            refresh_retained_exclusive(partial)
        }
    }
}

fn retained_exclusive_authority(
    retained: &ReplayAdmission,
    partial: &ReplayRetention,
) -> (
    HashSet<platform::Identity>,
    Vec<(RecoveryRegistration, ReplayCoordinate)>,
) {
    let mut identities = HashSet::new();
    let mut coordinates = Vec::new();
    for plan in &retained.plans {
        for coordinate in REPLAY_COORDINATES {
            if let ObservedEntry::File(file) = entry(plan, coordinate)
                && file.exclusive
            {
                identities.insert(file.identity);
                coordinates.push((plan.registration, coordinate));
            }
        }
    }
    for carrier in &partial.carriers {
        if let Some(file) = &carrier.file
            && file.exclusive
        {
            identities.insert(file.identity);
            coordinates.push((carrier.registration, carrier.coordinate));
        }
    }
    (identities, coordinates)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum TransferSource {
    Plan {
        plan: usize,
        coordinate: ReplayCoordinate,
    },
    Partial(usize),
}

struct CarrierTransfer {
    source: TransferSource,
    destination_plan: usize,
    destination_coordinate: ReplayCoordinate,
}

fn source_file<'a>(
    retained: &'a ReplayAdmission,
    partial: &'a ReplayRetention,
    source: TransferSource,
) -> &'a ObservedFile {
    match source {
        TransferSource::Plan { plan, coordinate } => match entry(&retained.plans[plan], coordinate)
        {
            ObservedEntry::File(file) => file,
            _ => unreachable!("validated recovery transfer source remains a file"),
        },
        TransferSource::Partial(index) => partial.carriers[index]
            .file
            .as_ref()
            .expect("validated retained partial carrier remains present"),
    }
}

fn transfer_retained_authority(
    root: &platform::RootGuard,
    retained: &mut ReplayAdmission,
    partial: &mut ReplayRetention,
    fresh: &mut ReplayAdmission,
) -> io::Result<()> {
    let mut transfers = Vec::new();
    let mut publication_transfers = Vec::new();
    for (destination_plan, plan) in fresh.plans.iter().enumerate() {
        for destination_coordinate in REPLAY_COORDINATES {
            let ObservedEntry::File(destination) = entry(plan, destination_coordinate) else {
                continue;
            };
            let pending_publication = retained.plans.iter().any(|source| {
                source.registration == plan.registration && source.publication.is_some()
            });
            if destination.exclusive
                || !(coordinate_requires_exclusive(plan, destination_coordinate)
                    || pending_publication)
            {
                continue;
            }
            let from_plan = retained.plans.iter().enumerate().find_map(|(index, source_plan)| {
                if source_plan.registration != plan.registration
                    || source_plan.record.operation_id != plan.record.operation_id
                {
                    return None;
                }
                REPLAY_COORDINATES
                .into_iter()
                .find(|coordinate| {
                    matches!(entry(source_plan, *coordinate), ObservedEntry::File(file) if file.exclusive && file.identity == destination.identity)
                })
                .map(|coordinate| TransferSource::Plan {
                    plan: index,
                    coordinate,
                })
            });
            let source = from_plan.or_else(|| {
                partial
                    .carriers
                    .iter()
                    .enumerate()
                    .find(|(_, source)| {
                        source.registration == plan.registration
                            && source.file.as_ref().is_some_and(|file| {
                                file.exclusive && file.identity == destination.identity
                            })
                    })
                    .map(|(index, _)| TransferSource::Partial(index))
            });
            let source = source.ok_or_else(|| {
                invalid("recovery replan lost retained exclusive carrier authority")
            })?;
            if transfers
                .iter()
                .any(|transfer: &CarrierTransfer| transfer.source == source)
            {
                return Err(invalid("recovery replan aliases a retained carrier"));
            }
            let source_file = source_file(retained, partial, source);
            let name = coordinate_name(plan, destination_coordinate);
            validate_parent_chain(root, plan)?;
            if source_file.receipt != destination.receipt
                || platform::file_identity(&source_file.handle)? != destination.identity
                || platform::file_receipt_fields(&source_file.handle)? != destination.receipt
                || platform::file_binding_state(
                    &plan.parent,
                    OsStr::new(name.as_str()),
                    destination.identity,
                )? != BindingState::Exact
            {
                return Err(identity_changed(
                    "recovery replan carrier transfer changed binding",
                ));
            }
            transfers.push(CarrierTransfer {
                source,
                destination_plan,
                destination_coordinate,
            });
            #[cfg(test)]
            if take_replay_transfer_failure() {
                return Err(io::Error::other(
                    "injected recovery transfer preflight failure",
                ));
            }
        }
    }

    for (source, source_plan) in retained.plans.iter().enumerate() {
        let Some(receipt) = source_plan.publication.as_ref() else {
            continue;
        };
        let Some((destination, destination_plan)) =
            exactly_one(fresh.plans.iter().enumerate().filter(|(_, plan)| {
                plan.registration == source_plan.registration
                    && plan.record.operation_id == source_plan.record.operation_id
            }))
        else {
            return Err(invalid(
                "recovery replan publication has no unique successor",
            ));
        };
        let stage_name = recovery_stage_leaf(destination_plan.record.operation_id);
        let Some(destination_file) =
            exactly_one(entries(destination_plan).into_iter().filter_map(|entry| {
                match entry {
                    ObservedEntry::File(file)
                        if receipt
                            .validate_recovery_binding(
                                publication_attempt(&destination_plan.record),
                                &file.handle,
                                &destination_plan.parent,
                                OsStr::new(stage_name.as_str()),
                                OsStr::new(destination_plan.record.destination_leaf.as_str()),
                            )
                            .is_ok() =>
                    {
                        Some(file)
                    }
                    _ => None,
                }
            }))
        else {
            return Err(invalid(
                "recovery replan publication lost its unique bound carrier",
            ));
        };
        let Some(source_file) =
            exactly_one(
                entries(source_plan)
                    .into_iter()
                    .filter_map(|entry| match entry {
                        ObservedEntry::File(file) if file.identity == destination_file.identity => {
                            Some(file)
                        }
                        _ => None,
                    }),
            )
        else {
            return Err(invalid(
                "recovery replan publication lost its retained carrier",
            ));
        };
        if source_file.receipt != destination_file.receipt
            || receipt
                .validate_recovery_binding(
                    publication_attempt(&destination_plan.record),
                    &source_file.handle,
                    &destination_plan.parent,
                    OsStr::new(stage_name.as_str()),
                    OsStr::new(destination_plan.record.destination_leaf.as_str()),
                )
                .is_err()
        {
            return Err(invalid(
                "recovery replan publication receipt changed binding",
            ));
        }
        publication_transfers.push((source, destination));
    }

    for transfer in transfers {
        #[cfg(test)]
        REPLAY_TRANSFER_COMMITS.with(|count| count.set(count.get() + 1));
        let destination = entry_mut(
            &mut fresh.plans[transfer.destination_plan],
            transfer.destination_coordinate,
        );
        let destination_proof = match destination {
            ObservedEntry::File(file) => file.proof,
            _ => unreachable!("validated recovery transfer destination remains a file"),
        };
        let mut file = match transfer.source {
            TransferSource::Plan { plan, coordinate } => {
                let ObservedEntry::File(file) = std::mem::replace(
                    entry_mut(&mut retained.plans[plan], coordinate),
                    ObservedEntry::Unowned,
                ) else {
                    unreachable!("validated recovery transfer source remains a file")
                };
                file
            }
            TransferSource::Partial(index) => partial.carriers[index]
                .file
                .take()
                .expect("validated retained partial carrier remains present"),
        };
        file.proof = destination_proof;
        *destination = ObservedEntry::File(file);
    }
    for (source, destination) in publication_transfers {
        fresh.plans[destination].publication = retained.plans[source].publication.take();
    }
    Ok(())
}

fn replacement_action(plan: &ReplayPlan) -> io::Result<ReplacementAction> {
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
    .ok_or_else(|| invalid("replacement recovery topology is invalid"))
}

fn advance_replay_phase(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plan: &mut ReplayPlan,
    phase: RecoveryPhase,
) -> io::Result<()> {
    let mut intended = plan.record.clone();
    intended.phase = phase;
    validate_plan_snapshot(root, plan)?;
    journal.advance(lease, plan.registration, intended.clone())?;
    plan.record = intended;
    if phase == RecoveryPhase::RemovePrepared {
        plan.target = ObservedEntry::Unowned;
        plan.park = ObservedEntry::Unowned;
        plan.publication = None;
    } else if phase == RecoveryPhase::RemoveCommitted {
        plan.publication = None;
    }
    validate_plan_snapshot(root, plan)
}

const MAX_REPLACEMENT_REPLAY_ACTIONS: usize = 8;

fn refresh_moved_file(file: &mut ObservedFile, expected: platform::Identity) -> io::Result<()> {
    if platform::file_identity(&file.handle)? != expected {
        return Err(identity_changed("recovery renamed file changed identity"));
    }
    let receipt = platform::file_receipt_fields(&file.handle)?;
    if receipt.0 != file.receipt.0
        || !platform::file_content_stamp_matches(receipt.1, file.receipt.1)
    {
        return Err(identity_changed("recovery renamed file changed content"));
    }
    file.receipt = receipt;
    Ok(())
}

fn rename_replay_file(
    root: &platform::RootGuard,
    plans: &mut [ReplayPlan],
    index: usize,
    source: ReplayCoordinate,
    destination: ReplayCoordinate,
    expected: RecoveryFileProof,
) -> io::Result<()> {
    let plan = &plans[index];
    let parent = Arc::clone(&plan.parent);
    let source_name = coordinate_name(plan, source);
    let destination_name = coordinate_name(plan, destination);
    let ObservedEntry::File(file) = entry(plan, source) else {
        return Err(invalid("recovery rename source is absent"));
    };
    if !file.exclusive || !matches!(entry(plan, destination), ObservedEntry::Absent) {
        return Err(invalid("recovery rename lacks exact exclusive topology"));
    }
    let identity = file.identity;
    revalidate(&parent, &source_name, file, Some(expected))?;
    validate_plan_snapshot(root, plan)?;
    platform::rename_recovery_file_no_replace(
        &parent,
        OsStr::new(source_name.as_str()),
        &file.handle,
        identity,
        OsStr::new(destination_name.as_str()),
    )?;

    let plan = &mut plans[index];
    let moved = std::mem::replace(entry_mut(plan, source), ObservedEntry::Absent);
    let replaced = std::mem::replace(entry_mut(plan, destination), moved);
    debug_assert!(matches!(replaced, ObservedEntry::Absent));
    {
        let ObservedEntry::File(file) = entry_mut(plan, destination) else {
            unreachable!("recovery rename moved its retained file")
        };
        refresh_moved_file(file, identity)?;
    }
    refresh_parent_after_effect(root, plans, &parent)?;
    let ObservedEntry::File(file) = entry(&plans[index], destination) else {
        unreachable!("recovery renamed destination remains retained")
    };
    platform::settle_renamed_recovery_file(
        &parent,
        OsStr::new(source_name.as_str()),
        OsStr::new(destination_name.as_str()),
        &file.handle,
        identity,
    )?;
    validate_plan_snapshot(root, &plans[index])
}

fn remove_replay_file(
    root: &platform::RootGuard,
    plans: &mut [ReplayPlan],
    pending_removals: &mut Vec<PendingRemoval>,
    retired: &mut Vec<ObservedFile>,
    index: usize,
    coordinate: ReplayCoordinate,
    expected: Option<RecoveryFileProof>,
) -> io::Result<()> {
    let plan = &plans[index];
    let parent = Arc::clone(&plan.parent);
    let name = coordinate_name(plan, coordinate);
    let ObservedEntry::File(file) = entry(plan, coordinate) else {
        return Err(invalid("recovery removal source is absent"));
    };
    if !file.exclusive {
        return Err(invalid("recovery removal lacks exclusive authority"));
    }
    let identity = file.identity;
    revalidate(&parent, &name, file, expected)?;
    validate_plan_snapshot(root, plan)?;
    let ObservedEntry::File(file) = entry_mut(&mut plans[index], coordinate) else {
        unreachable!("validated recovery removal retains its file")
    };
    platform::remove_recoverable_stage(
        &parent,
        OsStr::new(name.as_str()),
        &mut file.handle,
        identity,
    )?;

    let removed = std::mem::replace(
        entry_mut(&mut plans[index], coordinate),
        ObservedEntry::Absent,
    );
    let ObservedEntry::File(removed) = removed else {
        unreachable!("recovery removal moved its retained file")
    };
    pending_removals.push(PendingRemoval {
        registration: plans[index].registration,
        coordinate,
        file: removed,
    });
    #[cfg(test)]
    if take_replay_removal_settlement_failure() {
        return Err(io::Error::other(
            "injected recovery removal settlement failure",
        ));
    }
    settle_pending_removals(root, plans, pending_removals, retired)
}

fn settle_pending_recovery_publication(
    root: &platform::RootGuard,
    plan: &mut ReplayPlan,
) -> io::Result<()> {
    if plan.publication.is_none() {
        return Ok(());
    }
    if plan
        .publication
        .as_ref()
        .is_some_and(platform::PublicationReceipt::is_attempted)
        && matches!(plan.stage, ObservedEntry::File(_))
    {
        return Ok(());
    }
    if !matches!(
        plan.record.phase,
        RecoveryPhase::PublishPrepared | RecoveryPhase::RemoveCommitted
    ) || !matches!(plan.stage, ObservedEntry::Absent)
    {
        return Err(invalid(
            "recovery publication receipt has invalid durable topology",
        ));
    }
    validate_plan_snapshot(root, plan)?;
    let ObservedEntry::File(target) = &plan.target else {
        return Err(invalid("recovery publication receipt lost its target"));
    };
    let stage_name = recovery_stage_leaf(plan.record.operation_id);
    platform::settle_recovery_publication(
        plan.publication
            .as_mut()
            .expect("recovery publication receipt remains present"),
        publication_attempt(&plan.record),
        &target.handle,
        &plan.parent,
        OsStr::new(stage_name.as_str()),
        OsStr::new(plan.record.destination_leaf.as_str()),
    )?;
    validate_plan_snapshot(root, plan)?;
    plan.publication.take();
    Ok(())
}

fn publish_replay_stage(
    root: &platform::RootGuard,
    plans: &mut [ReplayPlan],
    index: usize,
) -> io::Result<()> {
    let plan = &mut plans[index];
    let stage_name = recovery_stage_leaf(plan.record.operation_id);
    if plan.publication.is_none() {
        let ObservedEntry::File(stage) = &plan.stage else {
            return Err(invalid("recovery publication stage is absent"));
        };
        if !stage.exclusive || !matches!(plan.target, ObservedEntry::Absent) {
            return Err(invalid(
                "recovery publication lacks exact exclusive topology",
            ));
        }
        revalidate(&plan.parent, &stage_name, stage, plan.record.new)?;
        let (size, stamp) = stage.receipt;
        plan.publication = Some(platform::prepare_publication(
            publication_attempt(&plan.record),
            &stage.handle,
            size,
            stamp,
            &plan.parent,
            OsStr::new(stage_name.as_str()),
            &plan.parent,
            OsStr::new(plan.record.destination_leaf.as_str()),
        )?);
    }
    validate_plan_snapshot(root, plan)?;
    let parent = Arc::clone(&plan.parent);
    let destination_name = plan.record.destination_leaf.clone();
    let ObservedEntry::File(stage) = &plan.stage else {
        return Err(invalid("recovery publication stage is absent"));
    };
    if !stage.exclusive || !matches!(plan.target, ObservedEntry::Absent) {
        return Err(invalid(
            "recovery publication lacks exact exclusive topology",
        ));
    }
    let identity = stage.identity;
    let ReplayPlan {
        record,
        stage,
        publication,
        ..
    } = plan;
    let ObservedEntry::File(stage) = stage else {
        unreachable!("validated recovery publication retains its stage")
    };
    platform::rename_recovery_publication_no_replace(
        publication
            .as_mut()
            .expect("prepared recovery publication retains its receipt"),
        publication_attempt(record),
        &parent,
        OsStr::new(stage_name.as_str()),
        &stage.handle,
        OsStr::new(destination_name.as_str()),
    )?;

    let plan = &mut plans[index];
    plan.target = std::mem::replace(&mut plan.stage, ObservedEntry::Absent);
    let ObservedEntry::File(target) = &mut plan.target else {
        unreachable!("recovery publication moved its retained stage")
    };
    refresh_moved_file(target, identity)?;
    refresh_parent_after_effect(root, plans, &parent)?;
    settle_pending_recovery_publication(root, &mut plans[index])
}

fn settle_replay_parent(
    root: &platform::RootGuard,
    plans: &[ReplayPlan],
    index: usize,
) -> io::Result<()> {
    let parent = Arc::clone(&plans[index].parent);
    validate_plan_snapshot(root, &plans[index])?;
    #[cfg(unix)]
    {
        platform::sync_publication_directory(&parent)?;
        validate_plan_snapshot(root, &plans[index])?;
    }
    refresh_parent_after_effect(root, plans, &parent)
}

fn settle_replay_rename(
    root: &platform::RootGuard,
    plans: &[ReplayPlan],
    index: usize,
    source: ReplayCoordinate,
    destination: ReplayCoordinate,
) -> io::Result<()> {
    let plan = &plans[index];
    let source_name = coordinate_name(plan, source);
    let destination_name = coordinate_name(plan, destination);
    let ObservedEntry::File(file) = entry(plan, destination) else {
        return Err(invalid("recovery renamed destination is absent"));
    };
    validate_plan_snapshot(root, plan)?;
    platform::settle_renamed_recovery_file(
        &plan.parent,
        OsStr::new(source_name.as_str()),
        OsStr::new(destination_name.as_str()),
        &file.handle,
        file.identity,
    )?;
    refresh_parent_after_effect(root, plans, &plan.parent)
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum ReplayMode {
    Selected,
    Successor,
}

fn retire_replay_record(
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plan: &ReplayPlan,
    mode: ReplayMode,
) -> io::Result<()> {
    #[cfg(unix)]
    let retire = {
        let _ = mode;
        true
    };
    #[cfg(windows)]
    let retire = mode == ReplayMode::Successor;
    if retire && journal.record(plan.registration).is_some() {
        journal.clear(lease, plan.registration)?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the cold replay effect keeps linear authority and successor retirement policy explicit"
)]
fn replay_replacement(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plans: &mut [ReplayPlan],
    pending_removals: &mut Vec<PendingRemoval>,
    retired: &mut Vec<ObservedFile>,
    index: usize,
    mode: ReplayMode,
) -> io::Result<()> {
    for _ in 0..MAX_REPLACEMENT_REPLAY_ACTIONS {
        settle_pending_recovery_publication(root, &mut plans[index])?;
        let action = replacement_action(&plans[index])?;
        if action != ReplacementAction::PublishStage
            && plans[index]
                .publication
                .as_ref()
                .is_some_and(platform::PublicationReceipt::is_attempted)
        {
            plans[index].publication.take();
        }
        match action {
            ReplacementAction::Advance(RecoveryPhase::ReplacePrepared) => {
                if mode == ReplayMode::Successor {
                    return Err(codec_error());
                }
                advance_replay_phase(
                    root,
                    lease,
                    journal,
                    &mut plans[index],
                    RecoveryPhase::ReplacePrepared,
                )?;
            }
            ReplacementAction::Advance(RecoveryPhase::PublishPrepared) => {
                if mode == ReplayMode::Successor {
                    return Err(codec_error());
                }
                settle_replay_rename(
                    root,
                    plans,
                    index,
                    ReplayCoordinate::Target,
                    ReplayCoordinate::Park,
                )?;
                advance_replay_phase(
                    root,
                    lease,
                    journal,
                    &mut plans[index],
                    RecoveryPhase::PublishPrepared,
                )?;
            }
            ReplacementAction::Advance(RecoveryPhase::RemovePrepared) => {
                if mode == ReplayMode::Successor {
                    return Err(codec_error());
                }
                advance_replay_phase(
                    root,
                    lease,
                    journal,
                    &mut plans[index],
                    RecoveryPhase::RemovePrepared,
                )?;
            }
            ReplacementAction::Advance(RecoveryPhase::RemoveCommitted) => {
                if mode == ReplayMode::Successor {
                    return Err(codec_error());
                }
                settle_replay_parent(root, plans, index)?;
                advance_replay_phase(
                    root,
                    lease,
                    journal,
                    &mut plans[index],
                    RecoveryPhase::RemoveCommitted,
                )?;
            }
            ReplacementAction::Advance(RecoveryPhase::StagePrepared)
            | ReplacementAction::Advance(RecoveryPhase::StageSealed) => {
                return Err(codec_error());
            }
            ReplacementAction::RemoveStage => {
                let expected = (plans[index].record.phase != RecoveryPhase::StagePrepared)
                    .then_some(plans[index].record.new)
                    .flatten();
                remove_replay_file(
                    root,
                    plans,
                    pending_removals,
                    retired,
                    index,
                    ReplayCoordinate::Stage,
                    expected,
                )?;
            }
            ReplacementAction::ParkTarget => {
                let expected = plans[index]
                    .record
                    .old
                    .ok_or_else(|| invalid("replacement target has no old proof"))?;
                rename_replay_file(
                    root,
                    plans,
                    index,
                    ReplayCoordinate::Target,
                    ReplayCoordinate::Park,
                    expected,
                )?;
            }
            ReplacementAction::PublishStage => publish_replay_stage(root, plans, index)?,
            ReplacementAction::RestorePark => {
                let expected = plans[index]
                    .record
                    .old
                    .ok_or_else(|| invalid("replacement park has no old proof"))?;
                rename_replay_file(
                    root,
                    plans,
                    index,
                    ReplayCoordinate::Park,
                    ReplayCoordinate::Target,
                    expected,
                )?;
            }
            ReplacementAction::RemovePark => {
                let expected = plans[index].record.old;
                remove_replay_file(
                    root,
                    plans,
                    pending_removals,
                    retired,
                    index,
                    ReplayCoordinate::Park,
                    expected,
                )?;
            }
            ReplacementAction::NoEffect => {
                if matches!(
                    plans[index].record.phase,
                    RecoveryPhase::ReplacePrepared | RecoveryPhase::PublishPrepared
                ) {
                    settle_replay_parent(root, plans, index)?;
                    advance_replay_phase(
                        root,
                        lease,
                        journal,
                        &mut plans[index],
                        RecoveryPhase::RemovePrepared,
                    )?;
                    continue;
                }
                if mode == ReplayMode::Successor {
                    settle_replay_parent(root, plans, index)?;
                } else {
                    #[cfg(unix)]
                    settle_replay_parent(root, plans, index)?;
                }
                retire_replay_record(lease, journal, &plans[index], mode)?;
                return Ok(());
            }
            ReplacementAction::Applied => {
                if mode == ReplayMode::Successor {
                    settle_replay_parent(root, plans, index)?;
                } else {
                    #[cfg(unix)]
                    settle_replay_parent(root, plans, index)?;
                }
                retire_replay_record(lease, journal, &plans[index], mode)?;
                return Ok(());
            }
        }
    }
    Err(invalid("replacement recovery action bound was exhausted"))
}

fn replay_publication(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plans: &mut [ReplayPlan],
    index: usize,
    mode: ReplayMode,
) -> io::Result<()> {
    if plans[index].record.phase == RecoveryPhase::StageSealed {
        if mode == ReplayMode::Successor {
            return Err(codec_error());
        }
        advance_replay_phase(
            root,
            lease,
            journal,
            &mut plans[index],
            RecoveryPhase::PublishPrepared,
        )?;
    }
    if matches!(plans[index].stage, ObservedEntry::File(_))
        && matches!(plans[index].target, ObservedEntry::Absent)
    {
        publish_replay_stage(root, plans, index)?;
    } else if matches!(plans[index].stage, ObservedEntry::Absent)
        && matches!(plans[index].target, ObservedEntry::File(_))
    {
        if plans[index].publication.is_some() {
            settle_pending_recovery_publication(root, &mut plans[index])?;
        } else {
            let ObservedEntry::File(target) = &plans[index].target else {
                unreachable!("publication target remains retained")
            };
            revalidate(
                &plans[index].parent,
                &plans[index].record.destination_leaf,
                target,
                plans[index].record.new,
            )?;
        }
    } else {
        return Err(invalid("recovery publication topology changed"));
    }
    settle_replay_parent(root, plans, index)?;
    retire_replay_record(lease, journal, &plans[index], mode)?;
    #[cfg(windows)]
    if mode == ReplayMode::Selected && plans[index].record.phase != RecoveryPhase::RemoveCommitted {
        advance_replay_phase(
            root,
            lease,
            journal,
            &mut plans[index],
            RecoveryPhase::RemoveCommitted,
        )?;
    }
    Ok(())
}

#[expect(
    clippy::too_many_arguments,
    reason = "the cold replay effect keeps linear authority and successor retirement policy explicit"
)]
fn replay_removal(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    plans: &mut [ReplayPlan],
    pending_removals: &mut Vec<PendingRemoval>,
    retired: &mut Vec<ObservedFile>,
    index: usize,
    mode: ReplayMode,
) -> io::Result<()> {
    if plans[index].record.phase != RecoveryPhase::RemovePrepared {
        if mode == ReplayMode::Successor {
            return Err(codec_error());
        }
        if plans[index]
            .publication
            .as_ref()
            .is_some_and(platform::PublicationReceipt::is_attempted)
        {
            plans[index].publication.take();
        }
        advance_replay_phase(
            root,
            lease,
            journal,
            &mut plans[index],
            RecoveryPhase::RemovePrepared,
        )?;
    }
    if matches!(plans[index].stage, ObservedEntry::File(_)) {
        let expected = plans[index].record.new;
        remove_replay_file(
            root,
            plans,
            pending_removals,
            retired,
            index,
            ReplayCoordinate::Stage,
            expected,
        )?;
    }
    if mode == ReplayMode::Successor {
        settle_replay_parent(root, plans, index)?;
    } else {
        #[cfg(unix)]
        settle_replay_parent(root, plans, index)?;
    }
    retire_replay_record(lease, journal, &plans[index], mode)?;
    Ok(())
}

#[expect(
    clippy::result_large_err,
    reason = "cold effect errors return the complete armed replay admission"
)]
fn replay(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
    journal: &mut RecoveryJournal,
    mut admission: ReplayAdmission,
    mode: ReplayMode,
) -> Result<ReplayAdmission, (io::Error, ReplayAdmission)> {
    let result = (|| -> io::Result<()> {
        let (plans, pending_removals, retired) = (
            &mut admission.plans,
            &mut admission.pending_removals,
            &mut admission.retired,
        );
        for index in 0..plans.len() {
            if plans[index].record.old.is_some() {
                replay_replacement(
                    root,
                    lease,
                    journal,
                    plans,
                    pending_removals,
                    retired,
                    index,
                    mode,
                )?;
                continue;
            }
            match plans[index].record.phase {
                RecoveryPhase::StagePrepared => {
                    if matches!(plans[index].stage, ObservedEntry::File(_)) {
                        remove_replay_file(
                            root,
                            plans,
                            pending_removals,
                            retired,
                            index,
                            ReplayCoordinate::Stage,
                            None,
                        )?;
                    }
                    #[cfg(unix)]
                    {
                        settle_replay_parent(root, plans, index)?;
                        journal.clear(lease, plans[index].registration)?;
                    }
                }
                RecoveryPhase::StageSealed => {
                    if present(&plans[index].target) {
                        replay_removal(
                            root,
                            lease,
                            journal,
                            plans,
                            pending_removals,
                            retired,
                            index,
                            mode,
                        )?;
                    } else {
                        replay_publication(root, lease, journal, plans, index, mode)?;
                    }
                }
                RecoveryPhase::PublishPrepared => {
                    if matches!(&plans[index].stage, ObservedEntry::File(_))
                        && present(&plans[index].target)
                    {
                        replay_removal(
                            root,
                            lease,
                            journal,
                            plans,
                            pending_removals,
                            retired,
                            index,
                            mode,
                        )?;
                    } else {
                        replay_publication(root, lease, journal, plans, index, mode)?;
                    }
                }
                RecoveryPhase::RemovePrepared => {
                    replay_removal(
                        root,
                        lease,
                        journal,
                        plans,
                        pending_removals,
                        retired,
                        index,
                        mode,
                    )?;
                }
                RecoveryPhase::RemoveCommitted => {
                    replay_publication(root, lease, journal, plans, index, mode)?;
                }
                RecoveryPhase::ReplacePrepared => return Err(codec_error()),
            }
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            if mode == ReplayMode::Selected {
                admission
                    .plans
                    .retain(|plan| journal.record(plan.registration).is_some());
            }
            Ok(admission)
        }
        Err(error) => Err((error, admission)),
    }
}

type ReplaySuccess = (RecoveryJournal, Vec<crate::RecoveryOrphan>);

fn into_orphans(admission: ReplayAdmission) -> Vec<crate::RecoveryOrphan> {
    admission
        .plans
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
                files: entries(&plan)
                    .into_iter()
                    .filter_map(|entry| match entry {
                        ObservedEntry::File(file) => Some(file.identity),
                        _ => None,
                    })
                    .collect(),
            }
        })
        .collect()
}

impl RecoveryReplay {
    pub(crate) fn state_successor(&self) -> io::Result<Option<StateSuccessorDescriptor>> {
        self.inner
            .as_ref()
            .expect("armed recovery replay retains its inner owner")
            .journal
            .state_successor()
    }

    pub(crate) fn resume_state_successor(
        mut self,
        root: &platform::RootGuard,
        lease: &platform::LeaseHandle,
    ) -> Result<ReplaySuccess, (io::Error, Self)> {
        if let Err(error) =
            platform::validate_lease(lease).and_then(|()| platform::validate_root(root))
        {
            return Err((error, self));
        }
        let inner = self
            .inner
            .as_mut()
            .expect("armed recovery replay retains its inner owner");
        let successor_owner_settlement = matches!(
            inner.state.as_ref(),
            Some(ReplayState::Successor {
                effects_complete: true,
                ..
            })
        );
        if !successor_owner_settlement && let Err(error) = inner.journal.reconcile_uncertain(lease)
        {
            return Err((error, self));
        }
        if let Some(state) = inner.state.as_mut()
            && let Err(error) = settle_replay_state_removals(root, state)
        {
            return Err((error, self));
        }
        let state = inner
            .state
            .take()
            .expect("armed recovery replay retains its state");
        let (mut owner, records, mut retained, mut partial, mut effects_complete) = match state {
            ReplayState::Successor {
                owner,
                records,
                retained,
                partial,
                effects_complete,
            } => (owner, records, retained, partial, effects_complete),
            ReplayState::Admit(partial) => {
                let descriptor = match inner.journal.state_successor() {
                    Ok(Some(descriptor)) => descriptor,
                    Ok(None) => {
                        inner.state = Some(ReplayState::Admit(partial));
                        return Err((invalid("State successor is absent"), self));
                    }
                    Err(error) => {
                        inner.state = Some(ReplayState::Admit(partial));
                        return Err((error, self));
                    }
                };
                let owner = match inner.journal.claim_state_successor() {
                    Ok(owner) => owner,
                    Err(error) => {
                        inner.state = Some(ReplayState::Admit(partial));
                        return Err((error, self));
                    }
                };
                (
                    Some(owner),
                    descriptor.recoveries,
                    ReplayAdmission::default(),
                    partial,
                    false,
                )
            }
            state @ ReplayState::Replan { .. } => {
                inner.state = Some(state);
                return Err((invalid("ordinary recovery replay is already active"), self));
            }
        };

        if !effects_complete {
            if let Err(error) = refresh_plan_exclusive(&mut retained)
                .and_then(|()| refresh_retained_exclusive(&mut partial))
                .and_then(|()| admit_replay_planning_attempt(&mut inner.planning_attempts))
            {
                inner.state = Some(ReplayState::Successor {
                    owner,
                    records,
                    retained,
                    partial,
                    effects_complete,
                });
                return Err((error, self));
            }
            let (exclusive, retained_coordinates) =
                retained_exclusive_authority(&retained, &partial);
            let mut fresh =
                match plan_replay_records(root, records.clone(), &exclusive, &retained_coordinates)
                {
                    Ok(admission) => admission,
                    Err(ReplayAdmissionFailure(error, admission)) => {
                        partial.absorb(admission);
                        inner.state = Some(ReplayState::Successor {
                            owner,
                            records,
                            retained,
                            partial,
                            effects_complete,
                        });
                        return Err((error, self));
                    }
                };
            if let Err(error) =
                transfer_retained_authority(root, &mut retained, &mut partial, &mut fresh)
            {
                partial.absorb(fresh);
                inner.state = Some(ReplayState::Successor {
                    owner,
                    records,
                    retained,
                    partial,
                    effects_complete,
                });
                return Err((error, self));
            }
            partial.absorb(retained);
            retained = match replay(
                root,
                lease,
                &mut inner.journal,
                fresh,
                ReplayMode::Successor,
            ) {
                Ok(admission) => admission,
                Err((error, admission)) => {
                    inner.state = Some(ReplayState::Successor {
                        owner,
                        records,
                        retained: admission,
                        partial,
                        effects_complete,
                    });
                    return Err((error, self));
                }
            };
            effects_complete = true;
        }

        let successor_owner = owner
            .take()
            .expect("accepted State successor retains its exact owner");
        if let Err((error, successor_owner)) =
            inner.journal.tombstone_successor(lease, successor_owner)
        {
            inner.state = Some(ReplayState::Successor {
                owner: Some(successor_owner),
                records,
                retained,
                partial,
                effects_complete,
            });
            return Err((error, self));
        }
        drop((retained, partial));
        inner.state = Some(ReplayState::Admit(ReplayRetention::default()));
        inner.planning_attempts = 0;
        self.resume(root, lease)
    }

    pub(crate) fn resume(
        mut self,
        root: &platform::RootGuard,
        lease: &platform::LeaseHandle,
    ) -> Result<ReplaySuccess, (io::Error, Self)> {
        if let Err(error) =
            platform::validate_lease(lease).and_then(|()| platform::validate_root(root))
        {
            return Err((error, self));
        }
        let inner = self
            .inner
            .as_mut()
            .expect("armed recovery replay retains its inner owner");
        if let Err(error) = inner.journal.reconcile_uncertain(lease) {
            return Err((error, self));
        }
        if inner.journal.has_live_successor() {
            return Err((
                io::Error::new(io::ErrorKind::InvalidData, "live successor"),
                self,
            ));
        }
        let state = inner
            .state
            .as_mut()
            .expect("armed recovery replay retains its state");
        if let Err(error) = settle_replay_state_removals(root, state) {
            return Err((error, self));
        }
        align_retained(&inner.journal, state);
        if inner.journal.records().next().is_none() {
            let inner = self
                .inner
                .take()
                .expect("empty recovery replay remains armed");
            return Ok((inner.journal, Vec::new()));
        }
        if let Err(error) = refresh_replay_state_exclusive(state) {
            return Err((error, self));
        }
        if let Err(error) = admit_replay_planning_attempt(&mut inner.planning_attempts) {
            return Err((error, self));
        }
        let state = inner
            .state
            .take()
            .expect("armed recovery replay retains its state");
        let (had_prior, mut retained, mut partial) = match state {
            ReplayState::Admit(partial) => (false, ReplayAdmission::default(), partial),
            ReplayState::Replan { retained, partial } => (true, retained, partial),
            state @ ReplayState::Successor { .. } => {
                inner.state = Some(state);
                return Err((
                    invalid("State successor replay requires domain admission"),
                    self,
                ));
            }
        };
        let (exclusive, retained_coordinates) = retained_exclusive_authority(&retained, &partial);
        let mut fresh = match plan_replay(root, &inner.journal, &exclusive, &retained_coordinates) {
            Ok(admission) => admission,
            Err(ReplayAdmissionFailure(error, admission)) => {
                partial.absorb(admission);
                inner.state = Some(retained_replay_state(had_prior, retained, partial));
                return Err((error, self));
            }
        };
        if let Err(error) =
            transfer_retained_authority(root, &mut retained, &mut partial, &mut fresh)
        {
            partial.absorb(fresh);
            inner.state = Some(retained_replay_state(had_prior, retained, partial));
            return Err((error, self));
        }
        partial.absorb(retained);
        #[cfg(test)]
        run_replay_parent_validation_hook();
        match replay(root, lease, &mut inner.journal, fresh, ReplayMode::Selected) {
            Ok(admission) => {
                let orphans = into_orphans(admission);
                let inner = self
                    .inner
                    .take()
                    .expect("successful recovery replay remains armed");
                Ok((inner.journal, orphans))
            }
            Err((error, retained)) => {
                inner.state = Some(ReplayState::Replan { retained, partial });
                Err((error, self))
            }
        }
    }

    pub(crate) fn acknowledge(mut self) {
        drop(self.inner.take());
    }
}

impl Drop for RecoveryReplay {
    fn drop(&mut self) {
        if self.inner.is_some() {
            std::process::abort();
        }
    }
}

pub(crate) fn initialize_and_replay(
    root: &platform::RootGuard,
    lease: &platform::LeaseHandle,
) -> Result<ReplaySuccess, (io::Error, Option<RecoveryReplay>)> {
    let journal = RecoveryJournal::load(lease).map_err(|error| (error, None))?;
    RecoveryReplay {
        inner: Some(Box::new(RecoveryReplayInner {
            journal,
            state: Some(ReplayState::Admit(ReplayRetention::default())),
            planning_attempts: 0,
        })),
    }
    .resume(root, lease)
    .map_err(|(error, replay)| (error, Some(replay)))
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

    #[test]
    fn replay_planning_retry_bound_refuses_before_growth() {
        let mut attempts = 0;
        for _ in 0..MAX_REPLAY_PLANNING_ATTEMPTS {
            admit_replay_planning_attempt(&mut attempts).expect("bounded replay attempt");
        }
        for _ in 0..2 {
            assert_eq!(
                admit_replay_planning_attempt(&mut attempts)
                    .expect_err("exhausted replay attempts must refuse")
                    .kind(),
                io::ErrorKind::WouldBlock,
            );
            assert_eq!(attempts, MAX_REPLAY_PLANNING_ATTEMPTS);
        }
    }
}
