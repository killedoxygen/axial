use super::{
    ManagedCreateOnlyWriteFailure, ManagedDir, ManagedExactChildCleanup, ManagedFileGuard,
    ManagedTreeDirectory, hex_lower,
};
use crate::download::{
    CreateOnlyTransferTarget, ManagedTransferAuthority, ManagedTransferTerminalAuthority,
    TransferByteContract, TransferContract, TransferReport, TransferTargetCancelObligation,
    TransferTargetCancelOutcome, VerifiedCreateOnly, VerifiedTransferDiscardObligation,
    VerifiedTransferDiscardOutcome,
};
use crate::loaders::LoaderError;
use crate::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
};
use axial_fs::{
    FileCapability, LeafName, TransientPublicationBatch, TransientPublicationBatchObligation,
    TransientPublicationBatchOutcome, TransientPublicationMember,
};
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fmt;
use std::io;
use std::sync::Arc;

const MANIFEST_NAME: &str = "axial.content.json";
const MAX_MANIFEST_BYTES: usize = 4 * 1024 * 1024;
const MAX_CONTENT_PATHS: usize = 512;
// Bounds cumulative speculative observations independently from the final transaction.
const MAX_CONTENT_PLANNING_PATHS: usize = 8_704;
const MAX_CONTENT_FILE_BYTES: u64 = 1 << 30;
const MAX_CONTENT_TRANSACTION_BYTES: u64 = 4 << 30;
const MAX_CONTENT_PRIVATE_DIRECTORIES: usize = 16;
const PRIVATE_STAGE_NAME: &str = "stage";
const PRIVATE_BACKUP_NAME: &str = "backup";

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct ManagedContentPayloadId(String);

impl ManagedContentPayloadId {
    pub fn new(value: &str) -> Result<Self, ManagedContentPlanError> {
        let value = PortableFileName::new_exact(value)
            .map_err(|_| ManagedContentPlanError::InvalidPayloadId)?;
        if managed_content_name_is_reserved(&value) {
            return Err(ManagedContentPlanError::InvalidPayloadId);
        }
        Ok(Self(value.as_str().to_string()))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedContentObservedState {
    Absent,
    Exact { size: u64, sha512: Box<str> },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedContentPathObservation {
    path: PortableRelativePath,
    state: ManagedContentObservedState,
}

impl ManagedContentPathObservation {
    pub fn path(&self) -> &PortableRelativePath {
        &self.path
    }

    pub fn state(&self) -> &ManagedContentObservedState {
        &self.state
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ManagedContentPathResult {
    Preserve,
    Absent,
    Download(ManagedContentPayloadId),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagedContentPathMutation {
    path: PortableRelativePath,
    observed: ManagedContentObservedState,
    result: ManagedContentPathResult,
}

impl ManagedContentPathMutation {
    pub fn new(
        path: PortableRelativePath,
        observed: ManagedContentObservedState,
        result: ManagedContentPathResult,
    ) -> Self {
        Self {
            path,
            observed,
            result,
        }
    }

    pub fn path(&self) -> &PortableRelativePath {
        &self.path
    }

    pub fn observed(&self) -> &ManagedContentObservedState {
        &self.observed
    }

    pub fn result(&self) -> &ManagedContentPathResult {
        &self.result
    }
}

#[derive(Clone, Debug)]
pub struct ManagedContentPayloadPlan {
    id: ManagedContentPayloadId,
    contract: TransferContract,
}

impl ManagedContentPayloadPlan {
    pub fn new(id: ManagedContentPayloadId, contract: TransferContract) -> Self {
        Self { id, contract }
    }

    pub fn id(&self) -> &ManagedContentPayloadId {
        &self.id
    }

    pub fn contract(&self) -> &TransferContract {
        &self.contract
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentPlanError {
    Empty,
    TooManyPaths,
    InvalidPath,
    InvalidPayloadId,
    DuplicatePath,
    DuplicatePayloadId,
    ReservedName,
    MissingObservation,
    ObservationChanged,
    MissingPayload,
    UnusedPayload,
    DuplicatePayloadUse,
    MissingDigest,
    InvalidManifest,
    ManifestTooLarge,
    PayloadTooLarge,
    TransactionBudgetExceeded,
}

#[must_use = "encoded manifests remain bound to the observing content session"]
pub struct ManagedContentEncodedManifest {
    body: Box<[u8]>,
    session: Arc<()>,
}

impl fmt::Debug for ManagedContentEncodedManifest {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentEncodedManifest")
            .field("bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

pub struct ManagedContentMutationPlan {
    mutations: Vec<ManagedContentPathMutation>,
    payloads: Vec<ManagedContentPayloadPlan>,
    manifest: ManagedContentEncodedManifest,
}

impl fmt::Debug for ManagedContentMutationPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentMutationPlan")
            .field("paths", &self.mutations.len())
            .field("payloads", &self.payloads.len())
            .field("manifest_bytes", &self.manifest.body.len())
            .finish_non_exhaustive()
    }
}

impl ManagedContentMutationPlan {
    pub fn new(
        observations: &[ManagedContentPathObservation],
        mutations: Vec<ManagedContentPathMutation>,
        payloads: Vec<ManagedContentPayloadPlan>,
        manifest: ManagedContentEncodedManifest,
    ) -> Result<Self, ManagedContentPlanError> {
        if mutations.is_empty() || observations.is_empty() {
            return Err(ManagedContentPlanError::Empty);
        }
        if mutations.len() > MAX_CONTENT_PATHS
            || observations.len() > MAX_CONTENT_PATHS
            || payloads.len() > MAX_CONTENT_PATHS
        {
            return Err(ManagedContentPlanError::TooManyPaths);
        }
        let mut observed_by_path = BTreeMap::new();
        let mut aggregate_bytes = 0_u64;
        for observation in observations {
            validate_content_path(&observation.path)?;
            if observed_by_path
                .insert(observation.path.key(), observation)
                .is_some()
            {
                return Err(ManagedContentPlanError::DuplicatePath);
            }
            if let ManagedContentObservedState::Exact { size, .. } = &observation.state {
                aggregate_bytes = aggregate_bytes
                    .checked_add(*size)
                    .ok_or(ManagedContentPlanError::TransactionBudgetExceeded)?;
                if aggregate_bytes > MAX_CONTENT_TRANSACTION_BYTES {
                    return Err(ManagedContentPlanError::TransactionBudgetExceeded);
                }
            }
        }

        let mut payload_ids = BTreeSet::new();
        for payload in &payloads {
            if !payload_ids.insert(payload.id.clone()) {
                return Err(ManagedContentPlanError::DuplicatePayloadId);
            }
            if payload.contract.digests().expected_sha1().is_none()
                && payload.contract.digests().expected_sha512().is_none()
            {
                return Err(ManagedContentPlanError::MissingDigest);
            }
            let limit = transfer_contract_limit(&payload.contract);
            if limit > MAX_CONTENT_FILE_BYTES {
                return Err(ManagedContentPlanError::PayloadTooLarge);
            }
            aggregate_bytes = aggregate_bytes
                .checked_add(limit)
                .ok_or(ManagedContentPlanError::TransactionBudgetExceeded)?;
            if aggregate_bytes > MAX_CONTENT_TRANSACTION_BYTES {
                return Err(ManagedContentPlanError::TransactionBudgetExceeded);
            }
        }

        let mut mutation_paths = BTreeSet::new();
        let mut used_payloads = BTreeSet::new();
        for mutation in &mutations {
            validate_content_path(&mutation.path)?;
            let key = mutation.path.key();
            if !mutation_paths.insert(key.clone()) {
                return Err(ManagedContentPlanError::DuplicatePath);
            }
            let Some(observed) = observed_by_path.get(&key) else {
                return Err(ManagedContentPlanError::MissingObservation);
            };
            if observed.path != mutation.path {
                return Err(ManagedContentPlanError::MissingObservation);
            }
            if observed.state != mutation.observed {
                return Err(ManagedContentPlanError::ObservationChanged);
            }
            if let ManagedContentPathResult::Download(id) = &mutation.result {
                if !payload_ids.contains(id) {
                    return Err(ManagedContentPlanError::MissingPayload);
                }
                if !used_payloads.insert(id.clone()) {
                    return Err(ManagedContentPlanError::DuplicatePayloadUse);
                }
            }
        }
        if mutation_paths.len() != observed_by_path.len() {
            return Err(ManagedContentPlanError::MissingObservation);
        }
        if used_payloads.len() != payload_ids.len() {
            return Err(ManagedContentPlanError::UnusedPayload);
        }
        Ok(Self {
            mutations,
            payloads,
            manifest,
        })
    }
}

fn transfer_contract_limit(contract: &TransferContract) -> u64 {
    match contract.bytes() {
        TransferByteContract::Exact(value)
        | TransferByteContract::AtMost(value)
        | TransferByteContract::Below(value) => value.get(),
    }
}

fn validate_content_path(path: &PortableRelativePath) -> Result<(), ManagedContentPlanError> {
    let mut segments = path.as_str().split('/');
    let Some(parent) = segments.next() else {
        return Err(ManagedContentPlanError::InvalidPath);
    };
    let Some(name) = segments.next() else {
        return Err(ManagedContentPlanError::InvalidPath);
    };
    if segments.next().is_some()
        || !matches!(parent, "mods" | "resourcepacks" | "shaderpacks")
    {
        return Err(ManagedContentPlanError::InvalidPath);
    }
    let name = PortableFileName::new_exact(name)
        .map_err(|_| ManagedContentPlanError::InvalidPath)?;
    if managed_content_name_is_reserved(&name) {
        return Err(ManagedContentPlanError::ReservedName);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentObservationError {
    Empty,
    TooManyPaths,
    InvalidPath,
    DuplicatePath,
    ParentUnavailable,
    MissingObservation,
    NonPortableEntry,
    FileUnavailable,
    FileTooLarge,
    ManifestTooLarge,
    TransactionBudgetExceeded,
}

#[must_use = "a refused manifest observation retains the transaction root"]
pub struct ManagedContentManifestObservationFailure {
    error: ManagedContentObservationError,
    root: ManagedContentTransactionRoot,
}

impl ManagedContentManifestObservationFailure {
    pub fn error(&self) -> ManagedContentObservationError {
        self.error
    }

    pub fn into_root(self) -> ManagedContentTransactionRoot {
        self.root
    }
}

impl fmt::Debug for ManagedContentManifestObservationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentManifestObservationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

struct ExactObservation {
    state: ManagedContentObservedState,
    guard: Option<ManagedFileGuard>,
    bytes: Option<Box<[u8]>>,
}

struct PathObservationAuthority {
    public: ManagedContentPathObservation,
    parent: ManagedDir,
    name: PortableFileName,
    guard: Option<ManagedFileGuard>,
}

#[must_use = "content planning retains exact manifest and filesystem authority"]
pub struct ManagedContentPlanningSession {
    root: ManagedDir,
    authority: ManagedTransferAuthority,
    manifest: ExactObservation,
    manifest_session: Arc<()>,
    observations: Vec<PathObservationAuthority>,
    observed_paths: BTreeMap<PortablePathKey, PortableRelativePath>,
    remaining_bytes: u64,
}

impl fmt::Debug for ManagedContentPlanningSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentPlanningSession")
            .field("paths", &self.observations.len())
            .finish_non_exhaustive()
    }
}

impl ManagedContentPlanningSession {
    pub fn manifest_state(&self) -> &ManagedContentObservedState {
        &self.manifest.state
    }

    pub fn manifest_bytes(&self) -> Option<&[u8]> {
        self.manifest.bytes.as_deref()
    }

    pub fn observations(&self) -> Vec<ManagedContentPathObservation> {
        self.observations
            .iter()
            .map(|observation| observation.public.clone())
            .collect()
    }

    pub fn observe_more(
        self,
        paths: Vec<PortableRelativePath>,
    ) -> Result<Self, ManagedContentPlanningObservationFailure> {
        observe_more_transaction_paths(self, paths)
    }

    pub fn finish(
        self,
        paths: Vec<PortableRelativePath>,
    ) -> Result<ManagedContentTransactionSession, ManagedContentPlanningObservationFailure> {
        finish_transaction_observation(self, paths)
    }
}

#[must_use = "a refused planning observation retains the exact planning session"]
pub struct ManagedContentPlanningObservationFailure {
    error: ManagedContentObservationError,
    session: ManagedContentPlanningSession,
}

impl ManagedContentPlanningObservationFailure {
    pub fn error(&self) -> ManagedContentObservationError {
        self.error
    }

    pub fn into_session(self) -> ManagedContentPlanningSession {
        self.session
    }
}

impl fmt::Debug for ManagedContentPlanningObservationFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentPlanningObservationFailure")
            .field("error", &self.error)
            .finish_non_exhaustive()
    }
}

#[must_use = "content observations retain exact filesystem authority"]
pub struct ManagedContentTransactionSession {
    root: ManagedDir,
    authority: ManagedTransferAuthority,
    manifest: ExactObservation,
    observations: Vec<PathObservationAuthority>,
    manifest_session: Arc<()>,
}

impl fmt::Debug for ManagedContentTransactionSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransactionSession")
            .field("paths", &self.observations.len())
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransactionSession {
    pub fn bind_encoded_manifest(
        &self,
        body: Vec<u8>,
    ) -> Result<ManagedContentEncodedManifest, ManagedContentPlanError> {
        if body.is_empty() {
            return Err(ManagedContentPlanError::InvalidManifest);
        }
        if body.len() > MAX_MANIFEST_BYTES {
            return Err(ManagedContentPlanError::ManifestTooLarge);
        }
        Ok(ManagedContentEncodedManifest {
            body: body.into_boxed_slice(),
            session: Arc::clone(&self.manifest_session),
        })
    }

    pub fn observations(&self) -> Vec<ManagedContentPathObservation> {
        self.observations
            .iter()
            .map(|observation| observation.public.clone())
            .collect()
    }

    pub fn prepare(
        self,
        plan: ManagedContentMutationPlan,
    ) -> ManagedContentPreparationOutcome {
        prepare_transaction(self, plan)
    }
}

#[must_use = "managed content transaction authority must be retained through settlement"]
pub struct ManagedContentTransactionRoot {
    directory: ManagedTreeDirectory,
    authority: ManagedTransferAuthority,
}

impl fmt::Debug for ManagedContentTransactionRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransactionRoot")
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransactionRoot {
    pub fn bind(
        directory: ManagedTreeDirectory,
        authority: ManagedTransferAuthority,
    ) -> Self {
        Self {
            directory,
            authority,
        }
    }

    pub fn observe_manifest(
        self,
    ) -> Result<ManagedContentPlanningSession, ManagedContentManifestObservationFailure> {
        observe_transaction_manifest(self)
    }
}

fn observe_transaction_manifest(
    transaction_root: ManagedContentTransactionRoot,
) -> Result<ManagedContentPlanningSession, ManagedContentManifestObservationFailure> {
    let refuse = |error, root| ManagedContentManifestObservationFailure { error, root };
    let ManagedContentTransactionRoot {
        directory: ManagedTreeDirectory { directory: root },
        authority,
    } = transaction_root;
    let manifest = match observe_file(
        &root,
        MANIFEST_NAME,
        MAX_MANIFEST_BYTES as u64,
        true,
        None,
    ) {
        Ok(observation) => observation,
        Err(error) => {
            return Err(refuse(
                public_observation_error(error, true),
                ManagedContentTransactionRoot {
                    directory: ManagedTreeDirectory { directory: root },
                    authority,
                },
            ));
        }
    };
    Ok(ManagedContentPlanningSession {
        root,
        authority,
        manifest,
        manifest_session: Arc::new(()),
        observations: Vec::new(),
        observed_paths: BTreeMap::new(),
        remaining_bytes: MAX_CONTENT_TRANSACTION_BYTES,
    })
}

fn observe_more_transaction_paths(
    mut session: ManagedContentPlanningSession,
    paths: Vec<PortableRelativePath>,
) -> Result<ManagedContentPlanningSession, ManagedContentPlanningObservationFailure> {
    let refuse = |error, session| ManagedContentPlanningObservationFailure { error, session };
    if paths.is_empty() {
        return Err(refuse(ManagedContentObservationError::Empty, session));
    }
    if session
        .observations
        .len()
        .checked_add(paths.len())
        .is_none_or(|total| total > MAX_CONTENT_PLANNING_PATHS)
    {
        return Err(refuse(
            ManagedContentObservationError::TooManyPaths,
            session,
        ));
    }
    let mut batch_keys = BTreeSet::new();
    for path in &paths {
        if validate_content_path(path).is_err() {
            return Err(refuse(ManagedContentObservationError::InvalidPath, session));
        }
        let key = path.key();
        if session.observed_paths.contains_key(&key)
            || !batch_keys.insert(key)
        {
            return Err(refuse(
                ManagedContentObservationError::DuplicatePath,
                session,
            ));
        }
    }

    for path in paths {
        let key = path.key();
        let exact_path = path.clone();
        let (parent_name, name) = split_content_path(&path);
        let parent = match session.root.open_child(parent_name) {
            Ok(parent) => parent,
            Err(_) => {
                return Err(refuse(
                    ManagedContentObservationError::ParentUnavailable,
                    session,
                ));
            }
        };
        let observed = match observe_file(
            &parent,
            name.as_str(),
            MAX_CONTENT_FILE_BYTES,
            false,
            Some(&mut session.remaining_bytes),
        ) {
            Ok(observed) => observed,
            Err(error) => {
                return Err(refuse(
                    public_observation_error(error, false),
                    session,
                ));
            }
        };
        session.observations.push(PathObservationAuthority {
            public: ManagedContentPathObservation {
                path,
                state: observed.state.clone(),
            },
            parent,
            name,
            guard: observed.guard,
        });
        let previous = session.observed_paths.insert(key, exact_path);
        debug_assert!(previous.is_none(), "prevalidated content path remains unique");
    }
    Ok(session)
}

fn finish_transaction_observation(
    session: ManagedContentPlanningSession,
    paths: Vec<PortableRelativePath>,
) -> Result<ManagedContentTransactionSession, ManagedContentPlanningObservationFailure> {
    if paths.is_empty() {
        return Err(ManagedContentPlanningObservationFailure {
            error: ManagedContentObservationError::Empty,
            session,
        });
    }
    if paths.len() > MAX_CONTENT_PATHS {
        return Err(ManagedContentPlanningObservationFailure {
            error: ManagedContentObservationError::TooManyPaths,
            session,
        });
    }
    let mut selected_keys = BTreeSet::new();
    for path in &paths {
        if validate_content_path(path).is_err() {
            return Err(ManagedContentPlanningObservationFailure {
                error: ManagedContentObservationError::InvalidPath,
                session,
            });
        }
        let key = path.key();
        if !selected_keys.insert(key.clone()) {
            return Err(ManagedContentPlanningObservationFailure {
                error: ManagedContentObservationError::DuplicatePath,
                session,
            });
        }
        if session.observed_paths.get(&key) != Some(path) {
            return Err(ManagedContentPlanningObservationFailure {
                error: ManagedContentObservationError::MissingObservation,
                session,
            });
        }
    }
    let ManagedContentPlanningSession {
        root,
        authority,
        manifest,
        manifest_session,
        observations,
        observed_paths: _,
        remaining_bytes: _,
    } = session;
    let mut observations_by_key = observations
        .into_iter()
        .map(|observation| (observation.public.path.key(), observation))
        .collect::<BTreeMap<_, _>>();
    let observations = paths
        .into_iter()
        .map(|path| {
            observations_by_key
                .remove(&path.key())
                .expect("validated final content path was inspected")
        })
        .collect();
    Ok(ManagedContentTransactionSession {
        root,
        authority,
        manifest,
        observations,
        manifest_session,
    })
}

#[derive(Clone, Copy)]
enum FileObservationFailure {
    NonPortableEntry,
    Unavailable,
    TooLarge,
    TransactionBudgetExceeded,
}

fn public_observation_error(
    error: FileObservationFailure,
    manifest: bool,
) -> ManagedContentObservationError {
    match error {
        FileObservationFailure::NonPortableEntry => {
            ManagedContentObservationError::NonPortableEntry
        }
        FileObservationFailure::Unavailable => ManagedContentObservationError::FileUnavailable,
        FileObservationFailure::TooLarge if manifest => {
            ManagedContentObservationError::ManifestTooLarge
        }
        FileObservationFailure::TooLarge => ManagedContentObservationError::FileTooLarge,
        FileObservationFailure::TransactionBudgetExceeded => {
            ManagedContentObservationError::TransactionBudgetExceeded
        }
    }
}

fn observe_file(
    parent: &ManagedDir,
    name: &str,
    max_bytes: u64,
    retain_bytes: bool,
    aggregate_remaining: Option<&mut u64>,
) -> Result<ExactObservation, FileObservationFailure> {
    let present = parent
        .has_portably_exact_child_name(name)
        .map_err(|error| match error {
            LoaderError::Verify(_) => FileObservationFailure::NonPortableEntry,
            _ => FileObservationFailure::Unavailable,
        })?;
    if !present {
        return Ok(ExactObservation {
            state: ManagedContentObservedState::Absent,
            guard: None,
            bytes: None,
        });
    }
    let guard = parent
        .inspect_regular_file(name)
        .map_err(|_| FileObservationFailure::Unavailable)?
        .ok_or(FileObservationFailure::Unavailable)?;
    if guard.size() > max_bytes {
        return Err(FileObservationFailure::TooLarge);
    }
    if let Some(remaining) = aggregate_remaining {
        admit_observed_bytes(remaining, guard.size())?;
    }
    let (sha512, bytes) = if retain_bytes {
        let bytes = parent
            .read_guarded_file_bounded(name, &guard, max_bytes)
            .map_err(|_| FileObservationFailure::Unavailable)?;
        let sha512 = hex_lower(&<[u8; 64]>::from(Sha512::digest(&bytes)));
        (sha512, Some(bytes.into_boxed_slice()))
    } else {
        (
            parent
                .sha512_guarded_file(name, &guard, max_bytes)
                .map_err(|_| FileObservationFailure::Unavailable)?,
            None,
        )
    };
    Ok(ExactObservation {
        state: ManagedContentObservedState::Exact {
            size: guard.size(),
            sha512: sha512.into_boxed_str(),
        },
        guard: Some(guard),
        bytes,
    })
}

fn admit_observed_bytes(
    remaining: &mut u64,
    size: u64,
) -> Result<(), FileObservationFailure> {
    *remaining = remaining
        .checked_sub(size)
        .ok_or(FileObservationFailure::TransactionBudgetExceeded)?;
    Ok(())
}

fn split_content_path(path: &PortableRelativePath) -> (&str, PortableFileName) {
    let (parent, name) = path
        .as_str()
        .split_once('/')
        .expect("validated content paths have one parent and one leaf");
    (
        parent,
        PortableFileName::new_exact(name).expect("validated content leaf remains portable"),
    )
}

struct ManagedContentTransferGroup {
    _state_authority: ManagedTransferAuthority,
}

struct ManagedContentTransferSlotAuthority {
    _transaction_authority: ManagedTransferAuthority,
}

#[must_use = "content transfer slots retain exact private destinations"]
pub struct ManagedContentTransferSlot {
    id: ManagedContentPayloadId,
    contract: TransferContract,
    target: CreateOnlyTransferTarget,
    cancellation: ManagedContentSlotCancellation,
}

impl fmt::Debug for ManagedContentTransferSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransferSlot")
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransferSlot {
    pub fn into_parts(
        self,
    ) -> (
        ManagedContentPayloadId,
        TransferContract,
        CreateOnlyTransferTarget,
        ManagedContentSlotCancellation,
    ) {
        (self.id, self.contract, self.target, self.cancellation)
    }
}

#[must_use = "issued slot cancellation must admit exact terminal transfer authority"]
pub struct ManagedContentSlotCancellation {
    id: ManagedContentPayloadId,
    authority: ManagedTransferAuthority,
}

impl fmt::Debug for ManagedContentSlotCancellation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentSlotCancellation")
            .finish_non_exhaustive()
    }
}

#[must_use = "cancelled slot receipts must settle their awaiting transaction"]
pub struct ManagedContentCancelledSlot {
    id: ManagedContentPayloadId,
    authority: ManagedTransferTerminalAuthority,
}

impl fmt::Debug for ManagedContentCancelledSlot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentCancelledSlot")
            .finish_non_exhaustive()
    }
}

#[must_use = "slot cancellation admission returns every rejected authority"]
pub enum ManagedContentSlotCancellationOutcome {
    Admitted(ManagedContentCancelledSlot),
    Refused {
        cancellation: ManagedContentSlotCancellation,
        authority: ManagedTransferTerminalAuthority,
    },
}

impl fmt::Debug for ManagedContentSlotCancellationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Admitted(_) => "Admitted",
            Self::Refused { .. } => "Refused",
        };
        formatter
            .debug_struct("ManagedContentSlotCancellationOutcome")
            .field("variant", &variant)
            .finish()
    }
}

impl ManagedContentSlotCancellation {
    pub fn admit(
        self,
        authority: ManagedTransferTerminalAuthority,
    ) -> ManagedContentSlotCancellationOutcome {
        if !authority.shares_retained_authority(&self.authority) {
            return ManagedContentSlotCancellationOutcome::Refused {
                cancellation: self,
                authority,
            };
        }
        ManagedContentSlotCancellationOutcome::Admitted(ManagedContentCancelledSlot {
            id: self.id,
            authority,
        })
    }
}

struct TransactionMutation {
    parent: ManagedDir,
    name: PortableFileName,
    old_guard: Option<ManagedFileGuard>,
    result: ManagedContentPathResult,
    backup_name: PortableFileName,
    installed_guard: Option<ManagedFileGuard>,
    claimed: bool,
    installed: bool,
}

struct PlannedPayload {
    id: ManagedContentPayloadId,
    contract: TransferContract,
    authority: ManagedTransferAuthority,
}

struct StagedPayload {
    name: PortableFileName,
    report: TransferReport,
    guard: Option<ManagedFileGuard>,
}

struct TransactionState {
    root: ManagedDir,
    _authority: ManagedTransferAuthority,
    private_name: PortableFileName,
    private: ManagedDir,
    stage: ManagedDir,
    backup: ManagedDir,
    manifest: ExactObservation,
    manifest_body: Box<[u8]>,
    mutations: Vec<TransactionMutation>,
    planned_payloads: Vec<PlannedPayload>,
    payload_by_id: BTreeMap<ManagedContentPayloadId, usize>,
    staged_by_id: BTreeMap<ManagedContentPayloadId, usize>,
    payloads: Vec<StagedPayload>,
    manifest_claimed: bool,
    manifest_installed: Option<ManagedFileGuard>,
    manifest_publication_started: bool,
    manifest_committed: bool,
    terminal_failure: ManagedContentTransactionFailure,
    stage_cleanup: CleanupDirectoryState,
    backup_cleanup: CleanupDirectoryState,
    private_cleanup: CleanupDirectoryState,
}

enum CleanupDirectoryState {
    Discover,
    Known(ManagedDir),
    Done,
}

#[must_use = "prepared content transactions retain private reservations"]
pub struct ManagedContentPreparedTransaction {
    state: TransactionState,
    slots: Vec<ManagedContentTransferSlot>,
}

#[must_use = "an awaiting content transaction must accept its exact verified slots"]
pub struct ManagedContentAwaitingTransaction {
    state: TransactionState,
}

impl fmt::Debug for ManagedContentAwaitingTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentAwaitingTransaction")
            .field("payloads", &self.state.planned_payloads.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ManagedContentPreparedTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentPreparedTransaction")
            .field("payloads", &self.state.planned_payloads.len())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentPreparationError {
    PlanDoesNotMatchObservation,
    PrivateNamespaceUnavailable,
    PrivateNamespaceExhausted,
}

#[must_use = "content preparation effects must be terminal or retained"]
pub enum ManagedContentPreparationOutcome {
    Prepared(ManagedContentPreparedTransaction),
    Refused {
        error: ManagedContentPreparationError,
        session: ManagedContentTransactionSession,
    },
    RecoveryRequired(ManagedContentRecovery),
}

impl fmt::Debug for ManagedContentPreparationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Prepared(_) => "Prepared",
            Self::Refused { .. } => "Refused",
            Self::RecoveryRequired(_) => "RecoveryRequired",
        };
        formatter
            .debug_struct("ManagedContentPreparationOutcome")
            .field("variant", &variant)
            .finish()
    }
}

fn prepare_transaction(
    session: ManagedContentTransactionSession,
    plan: ManagedContentMutationPlan,
) -> ManagedContentPreparationOutcome {
    if !plan_matches_session(&session, &plan) {
        return ManagedContentPreparationOutcome::Refused {
            error: ManagedContentPreparationError::PlanDoesNotMatchObservation,
            session,
        };
    }
    let private_entries = match session
        .root
        .entries_bounded(super::MAX_MANAGED_DIRECTORY_ENTRIES)
    {
        Ok(entries) => entries,
        Err(_) => {
            return ManagedContentPreparationOutcome::Refused {
                error: ManagedContentPreparationError::PrivateNamespaceUnavailable,
                session,
            };
        }
    };
    if private_entries.iter().filter(|entry| {
        entry
            .to_str()
            .is_some_and(|name| name.starts_with(".axial-content-"))
    }).count()
        >= MAX_CONTENT_PRIVATE_DIRECTORIES
    {
        return ManagedContentPreparationOutcome::Refused {
            error: ManagedContentPreparationError::PrivateNamespaceExhausted,
            session,
        };
    }
    let private_name = PortableFileName::new_exact(&format!(
        ".axial-content-{}",
        uuid::Uuid::new_v4().simple()
    ))
    .expect("generated content transaction name is portable");
    let private = match session.root.create_child_new(private_name.as_str()) {
        Ok(private) => private,
        Err(_) => {
            return ManagedContentPreparationOutcome::RecoveryRequired(
                ManagedContentRecovery::preparation(session, private_name),
            );
        }
    };
    let stage = match private.create_child_new(PRIVATE_STAGE_NAME) {
        Ok(stage) => stage,
        Err(_) => {
            return ManagedContentPreparationOutcome::RecoveryRequired(
                ManagedContentRecovery::private_cleanup(
                    session.root,
                    session.authority,
                    private_name,
                    private,
                    None,
                    None,
                ),
            );
        }
    };
    let backup = match private.create_child_new(PRIVATE_BACKUP_NAME) {
        Ok(backup) => backup,
        Err(_) => {
            return ManagedContentPreparationOutcome::RecoveryRequired(
                ManagedContentRecovery::private_cleanup(
                    session.root,
                    session.authority,
                    private_name,
                    private,
                    Some(stage),
                    None,
                ),
            );
        }
    };

    let group_authority = ManagedTransferAuthority::retain(Arc::new(
        ManagedContentTransferGroup {
            _state_authority: session.authority,
        },
    ));
    let mut slots = Vec::with_capacity(plan.payloads.len());
    let mut planned_payloads = Vec::with_capacity(plan.payloads.len());
    let mut payload_by_id = BTreeMap::new();
    if !plan.payloads.is_empty() {
        let names = plan
            .payloads
            .iter()
            .enumerate()
            .map(|(index, _)| {
                LeafName::new(format!("payload-{index}"))
                    .expect("bounded payload index is a portable leaf")
            })
            .collect();
        let destinations = match stage.inner.directory.admit_transient_destinations(names) {
            Ok(destinations) => destinations.into_destinations(),
            Err(_) => {
                return ManagedContentPreparationOutcome::RecoveryRequired(
                    ManagedContentRecovery::private_cleanup(
                        session.root,
                        group_authority,
                        private_name,
                        private,
                        Some(stage),
                        Some(backup),
                    ),
                );
            }
        };
        for ((index, payload), destination) in plan
            .payloads
            .iter()
            .enumerate()
            .zip(destinations)
        {
            let id = payload.id.clone();
            let slot_authority = ManagedTransferAuthority::retain(Arc::new(
                ManagedContentTransferSlotAuthority {
                    _transaction_authority: group_authority.retained(),
                },
            ));
            payload_by_id.insert(id.clone(), index);
            slots.push(ManagedContentTransferSlot {
                id: id.clone(),
                contract: payload.contract.clone(),
                target: CreateOnlyTransferTarget::new(
                    destination,
                    slot_authority.retained(),
                ),
                cancellation: ManagedContentSlotCancellation {
                    id: id.clone(),
                    authority: slot_authority.retained(),
                },
            });
            planned_payloads.push(PlannedPayload {
                id,
                contract: payload.contract.clone(),
                authority: slot_authority,
            });
            debug_assert!(index < MAX_CONTENT_PATHS);
        }
    }

    let mut mutations_by_key = plan
        .mutations
        .into_iter()
        .map(|mutation| (mutation.path.key(), mutation))
        .collect::<BTreeMap<_, _>>();
    let mutations = session
        .observations
        .into_iter()
        .enumerate()
        .map(|(index, observed)| {
            let mutation = mutations_by_key
                .remove(&observed.public.path.key())
                .expect("validated plan contains every observation");
            TransactionMutation {
                parent: observed.parent,
                name: observed.name,
                old_guard: observed.guard,
                result: mutation.result,
                backup_name: PortableFileName::new_exact(&format!("old-{index}"))
                    .expect("bounded backup index is portable"),
                installed_guard: None,
                claimed: false,
                installed: false,
            }
        })
        .collect();
    let stage_cleanup = CleanupDirectoryState::Known(stage.clone());
    let backup_cleanup = CleanupDirectoryState::Known(backup.clone());
    let private_cleanup = CleanupDirectoryState::Known(private.clone());
    ManagedContentPreparationOutcome::Prepared(ManagedContentPreparedTransaction {
        state: TransactionState {
            root: session.root,
            _authority: group_authority,
            private_name,
            private,
            stage,
            backup,
            manifest: session.manifest,
            manifest_body: plan.manifest.body,
            mutations,
            planned_payloads,
            payload_by_id,
            staged_by_id: BTreeMap::new(),
            payloads: Vec::new(),
            manifest_claimed: false,
            manifest_installed: None,
            manifest_publication_started: false,
            manifest_committed: false,
            terminal_failure: ManagedContentTransactionFailure::ObservationDrift,
            stage_cleanup,
            backup_cleanup,
            private_cleanup,
        },
        slots,
    })
}

fn plan_matches_session(
    session: &ManagedContentTransactionSession,
    plan: &ManagedContentMutationPlan,
) -> bool {
    if session.observations.len() != plan.mutations.len() {
        return false;
    }
    if !Arc::ptr_eq(&session.manifest_session, &plan.manifest.session) {
        return false;
    }
    let planned = plan
        .mutations
        .iter()
        .map(|mutation| (mutation.path.key(), mutation))
        .collect::<BTreeMap<_, _>>();
    session.observations.iter().all(|observation| {
        planned
            .get(&observation.public.path.key())
            .is_some_and(|mutation| {
                mutation.path == observation.public.path
                    && mutation.observed == observation.public.state
            })
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentStageError {
    MissingPayload,
    DuplicatePayload,
    ForeignAuthority,
    ContractMismatch,
    PublicationRefused,
}

#[must_use = "verified content stages must become ready or remain retained"]
pub enum ManagedContentStageOutcome {
    Ready(ManagedContentReadyTransaction),
    Refused {
        error: ManagedContentStageError,
        transaction: ManagedContentAwaitingTransaction,
        verified: Vec<(ManagedContentPayloadId, VerifiedCreateOnly)>,
    },
    RecoveryRequired(ManagedContentRecovery),
}

impl fmt::Debug for ManagedContentStageOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Ready(_) => "Ready",
            Self::Refused { .. } => "Refused",
            Self::RecoveryRequired(_) => "RecoveryRequired",
        };
        formatter
            .debug_struct("ManagedContentStageOutcome")
            .field("variant", &variant)
            .finish()
    }
}

impl ManagedContentPreparedTransaction {
    pub fn into_transfer_slots(
        self,
    ) -> (
        ManagedContentAwaitingTransaction,
        Vec<ManagedContentTransferSlot>,
    ) {
        let Self { state, slots } = self;
        (ManagedContentAwaitingTransaction { state }, slots)
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        let Self { state, slots } = self;
        cancel_transfer_slots(state, slots)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentCancellationError {
    MissingSlot,
    DuplicateSlot,
    ForeignAuthority,
}

#[must_use = "issued slot cancellation must be complete or returned to its transaction"]
pub enum ManagedContentCancellationOutcome {
    Accepted(ManagedContentTransactionOutcome),
    Refused {
        error: ManagedContentCancellationError,
        transaction: ManagedContentAwaitingTransaction,
        receipts: Vec<ManagedContentCancelledSlot>,
    },
}

impl fmt::Debug for ManagedContentCancellationOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Accepted(_) => "Accepted",
            Self::Refused { .. } => "Refused",
        };
        formatter
            .debug_struct("ManagedContentCancellationOutcome")
            .field("variant", &variant)
            .finish()
    }
}

impl ManagedContentAwaitingTransaction {
    pub fn accept_verified(
        self,
        verified: Vec<(ManagedContentPayloadId, VerifiedCreateOnly)>,
    ) -> ManagedContentStageOutcome {
        accept_verified(self, verified)
    }

    pub fn cancel(
        self,
        receipts: Vec<ManagedContentCancelledSlot>,
    ) -> ManagedContentCancellationOutcome {
        let mut seen = BTreeSet::new();
        let mut validation_error = None;
        for receipt in &receipts {
            let Some(index) = self.state.payload_by_id.get(&receipt.id).copied() else {
                validation_error = Some(ManagedContentCancellationError::ForeignAuthority);
                break;
            };
            if !seen.insert(receipt.id.clone()) {
                validation_error = Some(ManagedContentCancellationError::DuplicateSlot);
                break;
            }
            if !receipt
                .authority
                .shares_retained_authority(&self.state.planned_payloads[index].authority)
            {
                validation_error = Some(ManagedContentCancellationError::ForeignAuthority);
                break;
            }
        }
        if let Some(error) = validation_error {
            return ManagedContentCancellationOutcome::Refused {
                error,
                transaction: self,
                receipts,
            };
        }
        if receipts.len() != self.state.planned_payloads.len() {
            return ManagedContentCancellationOutcome::Refused {
                error: ManagedContentCancellationError::MissingSlot,
                transaction: self,
                receipts,
            };
        }
        drop(receipts);
        ManagedContentCancellationOutcome::Accepted(drive_rollback(self.state, true))
    }
}

fn accept_verified(
    transaction: ManagedContentAwaitingTransaction,
    verified: Vec<(ManagedContentPayloadId, VerifiedCreateOnly)>,
) -> ManagedContentStageOutcome {
    if verified.len() != transaction.state.planned_payloads.len() {
        return ManagedContentStageOutcome::Refused {
            error: ManagedContentStageError::MissingPayload,
            transaction,
            verified,
        };
    }
    let mut seen = BTreeSet::new();
    let mut validation_error = None;
    for (id, value) in &verified {
        let Some(index) = transaction.state.payload_by_id.get(id).copied() else {
            validation_error = Some(ManagedContentStageError::MissingPayload);
            break;
        };
        if !seen.insert(id.clone()) {
            validation_error = Some(ManagedContentStageError::DuplicatePayload);
            break;
        }
        let planned = &transaction.state.planned_payloads[index];
        if !value.shares_retained_authority(&planned.authority) {
            validation_error = Some(ManagedContentStageError::ForeignAuthority);
            break;
        }
        if !report_matches_contract(value.report(), &planned.contract) {
            validation_error = Some(ManagedContentStageError::ContractMismatch);
            break;
        }
    }
    if let Some(error) = validation_error {
        return ManagedContentStageOutcome::Refused {
            error,
            transaction,
            verified,
        };
    }
    let mut by_id = verified.into_iter().collect::<BTreeMap<_, _>>();
    let mut stages = Vec::with_capacity(transaction.state.planned_payloads.len());
    let mut retained = Vec::with_capacity(transaction.state.planned_payloads.len());
    for planned in &transaction.state.planned_payloads {
        let value = by_id
            .remove(&planned.id)
            .expect("complete verified set contains every planned payload");
        let (stage, report, authority) = value.into_content_stage();
        stages.push(stage);
        retained.push((planned.id.clone(), report, authority));
    }
    if stages.is_empty() {
        return ManagedContentStageOutcome::Ready(ManagedContentReadyTransaction {
            state: transaction.state,
        });
    }
    let batch = match TransientPublicationBatch::new(stages) {
        Ok(batch) => batch,
        Err(failure) => {
            let verified = failure
                .into_stages()
                .into_iter()
                .zip(retained)
                .map(|(stage, (id, report, authority))| {
                    (
                        id,
                        VerifiedCreateOnly::from_content_stage(stage, report, authority),
                    )
                })
                .collect();
            return ManagedContentStageOutcome::Refused {
                error: ManagedContentStageError::PublicationRefused,
                transaction,
                verified,
            };
        }
    };
    map_stage_publication(transaction.state, retained, batch.publish_create_new())
}

fn report_matches_contract(report: &TransferReport, contract: &TransferContract) -> bool {
    let bytes_match = match contract.bytes() {
        TransferByteContract::Exact(expected) => report.bytes() == expected.get(),
        TransferByteContract::AtMost(limit) => report.bytes() <= limit.get(),
        TransferByteContract::Below(limit) => report.bytes() < limit.get(),
    };
    let expected = contract.digests();
    let observed = report.digests();
    bytes_match
        && expected
            .expected_sha1()
            .is_none_or(|digest| observed.sha1() == Some(digest))
        && expected
            .expected_sha512()
            .is_none_or(|digest| observed.sha512() == Some(digest))
}

fn cancel_transfer_slots(
    state: TransactionState,
    slots: Vec<ManagedContentTransferSlot>,
) -> ManagedContentTransactionOutcome {
    let mut remaining = slots.into_iter();
    while let Some(slot) = remaining.next() {
        match slot.target.cancel() {
            TransferTargetCancelOutcome::Cancelled(authority) => drop(authority),
            TransferTargetCancelOutcome::Pending(obligation) => {
                return ManagedContentTransactionOutcome::RecoveryRequired(
                    ManagedContentRecovery {
                        state: Some(RecoveryState::TargetCancelPending {
                            transaction: state,
                            obligation: Some(obligation),
                            remaining: remaining.collect(),
                        }),
                    },
                );
            }
        }
    }
    drive_rollback(state, true)
}

fn map_stage_publication(
    mut state: TransactionState,
    retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
    outcome: TransientPublicationBatchOutcome,
) -> ManagedContentStageOutcome {
    match outcome {
        TransientPublicationBatchOutcome::Published(files) => {
            let mut members = files.into_iter().zip(retained).enumerate();
            while let Some((index, (file, (id, report, authority)))) = members.next() {
                let name = PortableFileName::new_exact(&format!("payload-{index}"))
                    .expect("bounded payload index is portable");
                let guard = match content_guard_from_file(
                    &state.stage,
                    LeafName::new(name.as_str()).expect("payload name is a native leaf"),
                    file,
                ) {
                    Ok(guard) => guard,
                    Err((_error, file)) => {
                        let mut remaining = vec![StageRecoveryMember::Published {
                            index,
                            id,
                            report,
                            authority,
                            file,
                        }];
                        remaining.extend(members.map(
                            |(index, (file, (id, report, authority)))| {
                                StageRecoveryMember::Published {
                                    index,
                                    id,
                                    report,
                                    authority,
                                    file,
                                }
                            },
                        ));
                        return ManagedContentStageOutcome::RecoveryRequired(
                            ManagedContentRecovery {
                                state: Some(RecoveryState::StageFilePending {
                                    transaction: state,
                                    remaining,
                                }),
                            },
                        );
                    }
                };
                state
                    .staged_by_id
                    .insert(id.clone(), state.payloads.len());
                state.payloads.push(StagedPayload {
                    name,
                    report,
                    guard: Some(guard),
                });
                drop(authority);
            }
            ManagedContentStageOutcome::Ready(ManagedContentReadyTransaction { state })
        }
        TransientPublicationBatchOutcome::NoEffect { batch, .. } => {
            let verified = batch
                .into_stages()
                .into_iter()
                .zip(retained)
                .map(|(stage, (id, report, authority))| {
                    (
                        id,
                        VerifiedCreateOnly::from_content_stage(stage, report, authority),
                    )
                })
                .collect();
            ManagedContentStageOutcome::Refused {
                error: ManagedContentStageError::PublicationRefused,
                transaction: ManagedContentAwaitingTransaction { state },
                verified,
            }
        }
        TransientPublicationBatchOutcome::Partial { members, .. } => {
            ManagedContentStageOutcome::RecoveryRequired(ManagedContentRecovery {
                state: Some(RecoveryState::StagePartial {
                    transaction: state,
                    retained,
                    members,
                }),
            })
        }
        TransientPublicationBatchOutcome::Pending(obligation) => {
            ManagedContentStageOutcome::RecoveryRequired(ManagedContentRecovery {
                state: Some(RecoveryState::StagePending {
                    transaction: state,
                    retained,
                    obligation: Some(obligation),
                }),
            })
        }
    }
}

fn content_guard_from_file(
    directory: &ManagedDir,
    name: LeafName,
    file: FileCapability,
) -> Result<ManagedFileGuard, (LoaderError, FileCapability)> {
    let revision = match file.revision() {
        Ok(revision) => revision,
        Err(error) => return Err((error.into(), file)),
    };
    let size = revision.size();
    let identity = directory
        .inner
        .root
        .intern_file(file, directory.inner.operation_pin.clone());
    Ok(ManagedFileGuard {
        directory: directory.inner.directory.clone(),
        name,
        identity,
        revision,
        size,
        _operation_pin: directory.inner.operation_pin.clone(),
    })
}

#[must_use = "ready content transactions must commit, cancel, or retain recovery"]
pub struct ManagedContentReadyTransaction {
    state: TransactionState,
}

impl fmt::Debug for ManagedContentReadyTransaction {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentReadyTransaction")
            .finish_non_exhaustive()
    }
}

impl ManagedContentReadyTransaction {
    pub fn commit(self) -> ManagedContentTransactionOutcome {
        drive_commit(self.state)
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        drive_rollback(self.state, true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentTransactionFailure {
    ObservationDrift,
    ClaimFailed,
    PayloadMoveFailed,
    SyncFailed,
    ManifestFailed,
    CleanupFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedContentCommitReceipt {
    path_count: usize,
    payload_count: usize,
}

impl ManagedContentCommitReceipt {
    pub fn path_count(self) -> usize {
        self.path_count
    }

    pub fn payload_count(self) -> usize {
        self.payload_count
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagedContentCancelReceipt {
    path_count: usize,
}

impl ManagedContentCancelReceipt {
    pub fn path_count(self) -> usize {
        self.path_count
    }
}

#[must_use = "content transaction outcomes retain every unsettled effect"]
pub enum ManagedContentTransactionOutcome {
    Committed(ManagedContentCommitReceipt),
    Cancelled(ManagedContentCancelReceipt),
    Failed(ManagedContentTransactionFailure),
    RecoveryRequired(ManagedContentRecovery),
}

impl fmt::Debug for ManagedContentTransactionOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Committed(_) => "Committed",
            Self::Cancelled(_) => "Cancelled",
            Self::Failed(_) => "Failed",
            Self::RecoveryRequired(_) => "RecoveryRequired",
        };
        formatter
            .debug_struct("ManagedContentTransactionOutcome")
            .field("variant", &variant)
            .finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum TransactionIntent {
    Commit,
    Cancel,
    Fail,
}

fn drive_commit(mut state: TransactionState) -> ManagedContentTransactionOutcome {
    if !revalidate_all(&state) {
        return drive_rollback(state, false);
    }
    for index in 0..state.mutations.len() {
        if matches!(
            &state.mutations[index].result,
            ManagedContentPathResult::Preserve
        ) || state.mutations[index].old_guard.is_none()
        {
            continue;
        }
        let mutation = &state.mutations[index];
        let guard = mutation.old_guard.as_ref().expect("exact observation has a guard");
        if mutation
            .parent
            .rename_guarded_file_no_replace(
                mutation.name.as_str(),
                guard,
                &state.backup,
                mutation.backup_name.as_str(),
            )
            .is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::ClaimFailed;
            return recovery(state, TransactionIntent::Fail);
        }
        state.mutations[index].claimed = true;
    }
    for index in 0..state.mutations.len() {
        let ManagedContentPathResult::Download(id) = &state.mutations[index].result else {
            continue;
        };
        let Some(payload_index) = state.staged_by_id.get(id).copied() else {
            return recovery(state, TransactionIntent::Fail);
        };
        let destination_parent = state.mutations[index].parent.clone();
        let destination_name = state.mutations[index].name.clone();
        let payload_name = state.payloads[payload_index].name.clone();
        if state
            .stage
            .rename_guarded_file_no_replace(
                payload_name.as_str(),
                state.payloads[payload_index]
                    .guard
                    .as_ref()
                    .expect("staged payload retains its exact guard"),
                &destination_parent,
                destination_name.as_str(),
            )
            .is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::PayloadMoveFailed;
            return recovery(state, TransactionIntent::Fail);
        }
        state.mutations[index].installed_guard = state.payloads[payload_index].guard.take();
        state.mutations[index].installed = true;
    }
    let mut synced = HashSet::new();
    for mutation in &state.mutations {
        if !matches!(&mutation.result, ManagedContentPathResult::Preserve)
            && (mutation.claimed || mutation.installed)
            && synced.insert(mutation.parent.inner.identity)
            && mutation.parent.sync().is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::SyncFailed;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    if let Some(guard) = state.manifest.guard.as_ref() {
        if state
            .root
            .rename_guarded_file_no_replace(
                MANIFEST_NAME,
                guard,
                &state.backup,
                "manifest-old",
            )
            .is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return recovery(state, TransactionIntent::Fail);
        }
        state.manifest_claimed = true;
    }
    state.manifest_publication_started = true;
    match state
        .root
        .write_new_exact_retained(MANIFEST_NAME, &state.manifest_body)
    {
        Ok(guard) => state.manifest_installed = Some(guard),
        Err(ManagedCreateOnlyWriteFailure::BeforePromotion(_error)) => {
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return drive_rollback(state, false);
        }
        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
            state.manifest_installed = final_guard;
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    if state.root.sync().is_err() {
        state.terminal_failure = ManagedContentTransactionFailure::SyncFailed;
        return recovery(state, TransactionIntent::Fail);
    }
    state.manifest_committed = state
        .manifest_installed
        .as_ref()
        .is_some_and(|guard| {
            state
                .root
                .read_guarded_file_bounded(
                    MANIFEST_NAME,
                    guard,
                    MAX_MANIFEST_BYTES as u64,
                )
                .is_ok_and(|body| body.as_slice() == state.manifest_body.as_ref())
        });
    if !state.manifest_committed {
        state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
        return recovery(state, TransactionIntent::Fail);
    }
    cleanup_committed(state)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ExactBindingState {
    Exact,
    Absent,
    Foreign,
    Unknown,
}

fn classify_exact_file(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
) -> ExactBindingState {
    match directory.file_guard_matches(name, guard) {
        Ok(true) => ExactBindingState::Exact,
        Ok(false) => match directory.has_portably_exact_child_name(name) {
            Ok(false) => ExactBindingState::Absent,
            Ok(true) => ExactBindingState::Foreign,
            Err(_) => ExactBindingState::Unknown,
        },
        Err(_) => ExactBindingState::Unknown,
    }
}

fn classify_name(directory: &ManagedDir, name: &str) -> ExactBindingState {
    match directory.has_portably_exact_child_name(name) {
        Ok(false) => ExactBindingState::Absent,
        Ok(true) => ExactBindingState::Foreign,
        Err(_) => ExactBindingState::Unknown,
    }
}

fn inspect_exact_file(
    directory: &ManagedDir,
    name: &str,
) -> Result<Option<ManagedFileGuard>, ()> {
    match classify_name(directory, name) {
        ExactBindingState::Absent => Ok(None),
        ExactBindingState::Foreign => directory
            .inspect_regular_file(name)
            .map_err(|_| ())?
            .map(Some)
            .ok_or(()),
        ExactBindingState::Exact => unreachable!("name-only classification cannot be exact"),
        ExactBindingState::Unknown => Err(()),
    }
}

fn revalidate_all(state: &TransactionState) -> bool {
    let manifest_matches = match state.manifest.guard.as_ref() {
        Some(guard) => {
            classify_exact_file(&state.root, MANIFEST_NAME, guard) == ExactBindingState::Exact
        }
        None => classify_name(&state.root, MANIFEST_NAME) == ExactBindingState::Absent,
    };
    manifest_matches
        && state.mutations.iter().all(|mutation| match mutation.old_guard.as_ref() {
            Some(guard) => {
                classify_exact_file(&mutation.parent, mutation.name.as_str(), guard)
                    == ExactBindingState::Exact
            }
            None => {
                classify_name(&mutation.parent, mutation.name.as_str())
                    == ExactBindingState::Absent
            }
        })
}

fn cleanup_committed(mut state: TransactionState) -> ManagedContentTransactionOutcome {
    for index in 0..state.mutations.len() {
        if !state.mutations[index].claimed {
            continue;
        }
        let removal_failed = {
            let mutation = &state.mutations[index];
            match mutation.old_guard.as_ref() {
                Some(guard) => state
                    .backup
                    .remove_guarded_file(mutation.backup_name.as_str(), guard)
                    .is_err(),
                None => true,
            }
        };
        if removal_failed {
            state.terminal_failure = ManagedContentTransactionFailure::CleanupFailed;
            return recovery(state, TransactionIntent::Commit);
        }
        state.mutations[index].claimed = false;
    }
    if state.manifest_claimed {
        let removal_failed = state.manifest.guard.as_ref().map_or(true, |guard| {
            state
                .backup
                .remove_guarded_file("manifest-old", guard)
                .is_err()
        });
        if removal_failed {
            state.terminal_failure = ManagedContentTransactionFailure::CleanupFailed;
            return recovery(state, TransactionIntent::Commit);
        }
        state.manifest_claimed = false;
    }
    let path_count = state.mutations.len();
    let payload_count = state.payloads.len();
    if cleanup_private(&mut state).is_err() {
        state.terminal_failure = ManagedContentTransactionFailure::CleanupFailed;
        return recovery(state, TransactionIntent::Commit);
    }
    ManagedContentTransactionOutcome::Committed(ManagedContentCommitReceipt {
        path_count,
        payload_count,
    })
}

fn drive_rollback(
    mut state: TransactionState,
    cancelled: bool,
) -> ManagedContentTransactionOutcome {
    if state.manifest_committed {
        return cleanup_committed(state);
    }
    if let Some(guard) = state.manifest_installed.as_ref() {
        if state.root.remove_guarded_file(MANIFEST_NAME, guard).is_err() {
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return recovery(
                state,
                if cancelled {
                    TransactionIntent::Cancel
                } else {
                    TransactionIntent::Fail
                },
            );
        }
        state.manifest_installed = None;
    }
    if state.manifest_claimed {
        let guard = state
            .manifest
            .guard
            .as_ref()
            .expect("claimed manifest has an exact observation");
        if state
            .backup
            .rename_guarded_file_no_replace("manifest-old", guard, &state.root, MANIFEST_NAME)
            .is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return recovery(
                state,
                if cancelled {
                    TransactionIntent::Cancel
                } else {
                    TransactionIntent::Fail
                },
            );
        }
        state.manifest_claimed = false;
    }
    for index in (0..state.mutations.len()).rev() {
        if state.mutations[index].installed {
            let guard = state.mutations[index]
                .installed_guard
                .as_ref()
                .expect("installed mutation retains its exact guard");
            if state.mutations[index]
                .parent
                .remove_guarded_file(state.mutations[index].name.as_str(), guard)
                .is_err()
            {
                state.terminal_failure = ManagedContentTransactionFailure::PayloadMoveFailed;
                return recovery(
                    state,
                    if cancelled {
                        TransactionIntent::Cancel
                    } else {
                        TransactionIntent::Fail
                    },
                );
            }
            state.mutations[index].installed = false;
        }
        if state.mutations[index].claimed {
            let mutation = &state.mutations[index];
            let guard = mutation
                .old_guard
                .as_ref()
                .expect("claimed mutation has an exact observation");
            if state
                .backup
                .rename_guarded_file_no_replace(
                    mutation.backup_name.as_str(),
                    guard,
                    &mutation.parent,
                    mutation.name.as_str(),
                )
                .is_err()
            {
                state.terminal_failure = ManagedContentTransactionFailure::ClaimFailed;
                return recovery(
                    state,
                    if cancelled {
                        TransactionIntent::Cancel
                    } else {
                        TransactionIntent::Fail
                    },
                );
            }
            state.mutations[index].claimed = false;
        }
    }
    let path_count = state.mutations.len();
    if cleanup_private(&mut state).is_err() {
        state.terminal_failure = ManagedContentTransactionFailure::CleanupFailed;
        return recovery(
            state,
            if cancelled {
                TransactionIntent::Cancel
            } else {
                TransactionIntent::Fail
            },
        );
    }
    if cancelled {
        ManagedContentTransactionOutcome::Cancelled(ManagedContentCancelReceipt { path_count })
    } else {
        ManagedContentTransactionOutcome::Failed(state.terminal_failure)
    }
}

fn cleanup_private(state: &mut TransactionState) -> Result<(), LoaderError> {
    for index in 0..state.payloads.len() {
        let Some(guard) = state.payloads[index].guard.take() else {
            continue;
        };
        let name = state.payloads[index].name.clone();
        match classify_exact_file(&state.stage, name.as_str(), &guard) {
            ExactBindingState::Exact => {
                if let Err(error) = state.stage.remove_guarded_file(name.as_str(), &guard) {
                    state.payloads[index].guard = Some(guard);
                    return Err(error);
                }
            }
            ExactBindingState::Absent => {}
            ExactBindingState::Foreign | ExactBindingState::Unknown => {
                state.payloads[index].guard = Some(guard);
                return Err(LoaderError::Verify(
                    "managed content stage cleanup is not classifiable".to_string(),
                ));
            }
        }
    }
    advance_cleanup_directory(&state.private, PRIVATE_STAGE_NAME, &mut state.stage_cleanup);
    if !matches!(&state.stage_cleanup, CleanupDirectoryState::Done) {
        return Err(LoaderError::Verify(
            "managed content stage cleanup remains unsettled".to_string(),
        ));
    }
    advance_cleanup_directory(
        &state.private,
        PRIVATE_BACKUP_NAME,
        &mut state.backup_cleanup,
    );
    if !matches!(&state.backup_cleanup, CleanupDirectoryState::Done) {
        return Err(LoaderError::Verify(
            "managed content backup cleanup remains unsettled".to_string(),
        ));
    }
    advance_cleanup_directory(
        &state.root,
        state.private_name.as_str(),
        &mut state.private_cleanup,
    );
    if matches!(&state.private_cleanup, CleanupDirectoryState::Done) {
        Ok(())
    } else {
        Err(LoaderError::Verify(
            "managed content private cleanup remains unsettled".to_string(),
        ))
    }
}

fn recovery(
    state: TransactionState,
    intent: TransactionIntent,
) -> ManagedContentTransactionOutcome {
    ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
        state: Some(RecoveryState::Transaction { state, intent }),
    })
}

enum RecoveryState {
    Preparation {
        session: ManagedContentTransactionSession,
        private_name: PortableFileName,
    },
    PrivateCleanup {
        root: ManagedDir,
        authority: ManagedTransferAuthority,
        private_name: PortableFileName,
        private: CleanupDirectoryState,
        stage: CleanupDirectoryState,
        backup: CleanupDirectoryState,
    },
    TargetCancelPending {
        transaction: TransactionState,
        obligation: Option<TransferTargetCancelObligation>,
        remaining: Vec<ManagedContentTransferSlot>,
    },
    StagePending {
        transaction: TransactionState,
        retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
        obligation: Option<TransientPublicationBatchObligation>,
    },
    StagePartial {
        transaction: TransactionState,
        retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
        members: Vec<TransientPublicationMember>,
    },
    StageDiscardPending {
        transaction: TransactionState,
        obligation: Option<VerifiedTransferDiscardObligation>,
        remaining: Vec<StageRecoveryMember>,
    },
    StageFilePending {
        transaction: TransactionState,
        remaining: Vec<StageRecoveryMember>,
    },
    Transaction {
        state: TransactionState,
        intent: TransactionIntent,
    },
}

#[must_use = "content transaction recovery must be reconciled explicitly"]
pub struct ManagedContentRecovery {
    state: Option<RecoveryState>,
}

impl fmt::Debug for ManagedContentRecovery {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentRecovery")
            .finish_non_exhaustive()
    }
}

impl ManagedContentRecovery {
    fn preparation(
        session: ManagedContentTransactionSession,
        private_name: PortableFileName,
    ) -> Self {
        Self {
            state: Some(RecoveryState::Preparation {
                session,
                private_name,
            }),
        }
    }

    fn private_cleanup(
        root: ManagedDir,
        authority: ManagedTransferAuthority,
        private_name: PortableFileName,
        private: ManagedDir,
        stage: Option<ManagedDir>,
        backup: Option<ManagedDir>,
    ) -> Self {
        Self {
            state: Some(RecoveryState::PrivateCleanup {
                root,
                authority,
                private_name,
                private: CleanupDirectoryState::Known(private),
                stage: stage.map_or(CleanupDirectoryState::Discover, CleanupDirectoryState::Known),
                backup: backup
                    .map_or(CleanupDirectoryState::Discover, CleanupDirectoryState::Known),
            }),
        }
    }

    pub fn reconcile(mut self) -> ManagedContentTransactionOutcome {
        match self
            .state
            .take()
            .expect("content recovery retains one exact state")
        {
            RecoveryState::Preparation {
                session,
                private_name,
            } => {
                if session.root.inner.root.settle().is_err() {
                    return ManagedContentTransactionOutcome::RecoveryRequired(
                        Self::preparation(session, private_name),
                    );
                }
                match session.root.open_child(private_name.as_str()) {
                    Ok(private) => {
                        let recovery = Self::private_cleanup(
                            session.root,
                            session.authority,
                            private_name,
                            private,
                            None,
                            None,
                        );
                        recovery.reconcile()
                    }
                    Err(LoaderError::Io(error)) if error.kind() == io::ErrorKind::NotFound => {
                        ManagedContentTransactionOutcome::Failed(
                            ManagedContentTransactionFailure::CleanupFailed,
                        )
                    }
                    Err(_) => ManagedContentTransactionOutcome::RecoveryRequired(Self::preparation(
                        session,
                        private_name,
                    )),
                }
            }
            RecoveryState::PrivateCleanup {
                root,
                authority,
                private_name,
                mut private,
                mut stage,
                mut backup,
            } => {
                if matches!(&private, CleanupDirectoryState::Discover) {
                    advance_cleanup_directory(&root, private_name.as_str(), &mut private);
                }
                let private_dir = match &private {
                    CleanupDirectoryState::Known(private_dir) => Some(private_dir.clone()),
                    _ => None,
                };
                if let Some(private_dir) = private_dir {
                    advance_cleanup_directory(&private_dir, PRIVATE_STAGE_NAME, &mut stage);
                    if matches!(&stage, CleanupDirectoryState::Done) {
                        advance_cleanup_directory(&private_dir, PRIVATE_BACKUP_NAME, &mut backup);
                    }
                    if matches!(&stage, CleanupDirectoryState::Done)
                        && matches!(&backup, CleanupDirectoryState::Done)
                    {
                        advance_cleanup_directory(&root, private_name.as_str(), &mut private);
                    }
                }
                if matches!(&private, CleanupDirectoryState::Done) {
                    drop((authority, private_name));
                    ManagedContentTransactionOutcome::Failed(
                        ManagedContentTransactionFailure::CleanupFailed,
                    )
                } else {
                    ManagedContentTransactionOutcome::RecoveryRequired(Self {
                        state: Some(RecoveryState::PrivateCleanup {
                            root,
                            authority,
                            private_name,
                            private,
                            stage,
                            backup,
                        }),
                    })
                }
            }
            RecoveryState::TargetCancelPending {
                transaction,
                mut obligation,
                remaining,
            } => match obligation
                .take()
                .expect("prepared cancellation retains its exact target obligation")
                .reconcile()
            {
                TransferTargetCancelOutcome::Cancelled(authority) => {
                    drop(authority);
                    cancel_transfer_slots(transaction, remaining)
                }
                TransferTargetCancelOutcome::Pending(obligation) => {
                    ManagedContentTransactionOutcome::RecoveryRequired(Self {
                        state: Some(RecoveryState::TargetCancelPending {
                            transaction,
                            obligation: Some(obligation),
                            remaining,
                        }),
                    })
                }
            },
            RecoveryState::StagePending {
                transaction,
                retained,
                mut obligation,
            } => map_stage_recovery(
                transaction,
                retained,
                obligation
                    .take()
                    .expect("stage recovery retains its publication obligation")
                    .reconcile(),
            ),
            RecoveryState::StagePartial {
                transaction,
                retained,
                members,
            } => recover_partial_stage(transaction, retained, members),
            RecoveryState::StageDiscardPending {
                transaction,
                mut obligation,
                remaining,
            } => match obligation
                .take()
                .expect("stage discard recovery retains its exact obligation")
                .reconcile()
            {
                VerifiedTransferDiscardOutcome::Discarded { .. } => {
                    drive_stage_cleanup(transaction, remaining)
                }
                VerifiedTransferDiscardOutcome::Pending(obligation) => {
                    ManagedContentTransactionOutcome::RecoveryRequired(Self {
                        state: Some(RecoveryState::StageDiscardPending {
                            transaction,
                            obligation: Some(obligation),
                            remaining,
                        }),
                    })
                }
            },
            RecoveryState::StageFilePending {
                transaction,
                remaining,
            } => drive_stage_cleanup(transaction, remaining),
            RecoveryState::Transaction { state, intent } => {
                let mut state = state;
                if state.root.inner.root.settle().is_err()
                    || !classify_transaction(&mut state)
                {
                    return recovery(state, intent);
                }
                if state.manifest_committed || intent == TransactionIntent::Commit {
                    cleanup_committed(state)
                } else {
                    drive_rollback(state, intent == TransactionIntent::Cancel)
                }
            }
        }
    }
}

fn advance_cleanup_directory(
    parent: &ManagedDir,
    name: &str,
    state: &mut CleanupDirectoryState,
) {
    let current = std::mem::replace(state, CleanupDirectoryState::Discover);
    *state = match current {
        CleanupDirectoryState::Discover => match parent.discover_exact_child(name) {
            Ok(Some(child)) => CleanupDirectoryState::Known(child),
            Ok(None) => CleanupDirectoryState::Done,
            Err(_) => CleanupDirectoryState::Discover,
        },
        CleanupDirectoryState::Known(child) => {
            match parent.settle_remove_exact_empty_child(name, child) {
                ManagedExactChildCleanup::Done => CleanupDirectoryState::Done,
                ManagedExactChildCleanup::Known(child) => CleanupDirectoryState::Known(child),
            }
        }
        CleanupDirectoryState::Done => CleanupDirectoryState::Done,
    };
}

fn classify_transaction(state: &mut TransactionState) -> bool {
    for mutation in &mut state.mutations {
        if matches!(&mutation.result, ManagedContentPathResult::Preserve) {
            if mutation.claimed || mutation.installed || mutation.installed_guard.is_some() {
                return false;
            }
            mutation.claimed = false;
            continue;
        }
        let Some(guard) = mutation.old_guard.as_ref() else {
            mutation.claimed = false;
            continue;
        };
        let source = classify_exact_file(&mutation.parent, mutation.name.as_str(), guard);
        let backup = classify_exact_file(&state.backup, mutation.backup_name.as_str(), guard);
        match (source, backup) {
            (ExactBindingState::Exact, ExactBindingState::Absent) => mutation.claimed = false,
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Exact) => {
                mutation.claimed = true;
            }
            (
                ExactBindingState::Absent | ExactBindingState::Foreign,
                ExactBindingState::Absent,
            ) if state.manifest_committed => mutation.claimed = false,
            _ => return false,
        }
    }

    if let Some(guard) = state.manifest.guard.as_ref() {
        let source = classify_exact_file(&state.root, MANIFEST_NAME, guard);
        let backup = classify_exact_file(&state.backup, "manifest-old", guard);
        match (source, backup) {
            (ExactBindingState::Exact, ExactBindingState::Absent) => {
                state.manifest_claimed = false;
            }
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Exact) => {
                state.manifest_claimed = true;
            }
            (
                ExactBindingState::Absent | ExactBindingState::Foreign,
                ExactBindingState::Absent,
            ) if state.manifest_committed => state.manifest_claimed = false,
            _ => return false,
        }
    } else {
        if !state.manifest_publication_started
            && classify_name(&state.root, MANIFEST_NAME) != ExactBindingState::Absent
        {
            return false;
        }
        state.manifest_claimed = false;
    }

    for mutation_index in 0..state.mutations.len() {
        let id = match &state.mutations[mutation_index].result {
            ManagedContentPathResult::Preserve => continue,
            ManagedContentPathResult::Absent => {
                if state.mutations[mutation_index].claimed || state.manifest_committed {
                    if classify_name(
                        &state.mutations[mutation_index].parent,
                        state.mutations[mutation_index].name.as_str(),
                    ) != ExactBindingState::Absent
                    {
                        return false;
                    }
                }
                continue;
            }
            ManagedContentPathResult::Download(id) => id,
        };
        let Some(payload_index) = state.staged_by_id.get(id).copied() else {
            return false;
        };
        let mut guard = state.mutations[mutation_index]
            .installed_guard
            .take()
            .or_else(|| state.payloads[payload_index].guard.take());
        if guard.is_none() {
            let staged = match inspect_exact_file(
                &state.stage,
                state.payloads[payload_index].name.as_str(),
            ) {
                Ok(value) => value,
                Err(()) => return false,
            };
            if let Some(staged) = staged {
                if !payload_guard_matches_report(
                    &state.stage,
                    state.payloads[payload_index].name.as_str(),
                    &staged,
                    &state.payloads[payload_index].report,
                ) {
                    return false;
                }
                guard = Some(staged);
            } else {
                let installed = match inspect_exact_file(
                    &state.mutations[mutation_index].parent,
                    state.mutations[mutation_index].name.as_str(),
                ) {
                    Ok(value) => value,
                    Err(()) => return false,
                };
                if let Some(installed) = installed {
                    if payload_guard_matches_report(
                        &state.mutations[mutation_index].parent,
                        state.mutations[mutation_index].name.as_str(),
                        &installed,
                        &state.payloads[payload_index].report,
                    ) {
                        guard = Some(installed);
                    } else if !destination_matches_prior(state, mutation_index) {
                        return false;
                    }
                }
            }
        }
        let Some(guard) = guard else {
            if state.manifest_committed || !destination_matches_prior(state, mutation_index) {
                return false;
            }
            state.mutations[mutation_index].installed = false;
            continue;
        };
        let staged = classify_exact_file(
            &state.stage,
            state.payloads[payload_index].name.as_str(),
            &guard,
        );
        let installed = classify_exact_file(
            &state.mutations[mutation_index].parent,
            state.mutations[mutation_index].name.as_str(),
            &guard,
        );
        match (staged, installed) {
            (ExactBindingState::Exact, ExactBindingState::Absent) => {
                state.payloads[payload_index].guard = Some(guard);
                state.mutations[mutation_index].installed = false;
            }
            (ExactBindingState::Exact, ExactBindingState::Foreign)
                if destination_matches_prior(state, mutation_index) =>
            {
                state.payloads[payload_index].guard = Some(guard);
                state.mutations[mutation_index].installed = false;
            }
            (ExactBindingState::Absent, ExactBindingState::Exact) => {
                state.mutations[mutation_index].installed_guard = Some(guard);
                state.mutations[mutation_index].installed = true;
            }
            (ExactBindingState::Absent, ExactBindingState::Absent)
                if !state.manifest_committed =>
            {
                state.mutations[mutation_index].installed = false;
            }
            _ => return false,
        }
    }

    if state.manifest_publication_started && !state.manifest_committed {
        if let Some(guard) = state.manifest_installed.take() {
            match classify_exact_file(&state.root, MANIFEST_NAME, &guard) {
                ExactBindingState::Exact => state.manifest_installed = Some(guard),
                ExactBindingState::Absent => {}
                ExactBindingState::Foreign | ExactBindingState::Unknown => return false,
            }
        }
        if state.manifest_installed.is_none() {
            let guard = match inspect_exact_file(&state.root, MANIFEST_NAME) {
                Ok(Some(guard)) => guard,
                Ok(None) => return true,
                Err(()) => return false,
            };
            let is_new = state
                .root
                .read_guarded_file_bounded(
                    MANIFEST_NAME,
                    &guard,
                    MAX_MANIFEST_BYTES as u64,
                )
                .is_ok_and(|body| body.as_slice() == state.manifest_body.as_ref());
            if is_new {
                state.manifest_installed = Some(guard);
            } else {
                return false;
            }
        }
        if state.manifest_installed.is_some() {
            if state.root.sync().is_err() {
                return false;
            }
            state.manifest_committed = true;
        }
    }
    true
}

fn destination_matches_prior(state: &TransactionState, mutation_index: usize) -> bool {
    let mutation = &state.mutations[mutation_index];
    match mutation.old_guard.as_ref() {
        Some(guard) if !mutation.claimed => {
            classify_exact_file(&mutation.parent, mutation.name.as_str(), guard)
                == ExactBindingState::Exact
        }
        _ => {
            classify_name(&mutation.parent, mutation.name.as_str())
                == ExactBindingState::Absent
        }
    }
}

fn payload_guard_matches_report(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    report: &TransferReport,
) -> bool {
    if guard.size() != report.bytes() {
        return false;
    }
    let digests = report.digests();
    let sha1_matches = digests.sha1().is_none_or(|expected| {
        directory
            .sha1_guarded_file_bytes(name, guard, MAX_CONTENT_FILE_BYTES)
            .is_ok_and(|observed| observed == *expected)
    });
    let sha512_matches = digests.sha512().is_none_or(|expected| {
        directory
            .sha512_guarded_file(name, guard, MAX_CONTENT_FILE_BYTES)
            .is_ok_and(|observed| observed == hex_lower(expected))
    });
    (digests.sha1().is_some() || digests.sha512().is_some())
        && sha1_matches
        && sha512_matches
}

fn map_stage_recovery(
    transaction: TransactionState,
    retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
    outcome: TransientPublicationBatchOutcome,
) -> ManagedContentTransactionOutcome {
    match outcome {
        TransientPublicationBatchOutcome::Pending(obligation) => {
            ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
                state: Some(RecoveryState::StagePending {
                    transaction,
                    retained,
                    obligation: Some(obligation),
                }),
            })
        }
        TransientPublicationBatchOutcome::Partial { members, .. } => recover_partial_stage(
            transaction,
            retained,
            members,
        ),
        TransientPublicationBatchOutcome::Published(files) => {
            let members = files
                .into_iter()
                .map(TransientPublicationMember::Published)
                .collect();
            recover_partial_stage(transaction, retained, members)
        }
        TransientPublicationBatchOutcome::NoEffect { batch, .. } => {
            let members = batch
                .into_stages()
                .into_iter()
                .map(TransientPublicationMember::Unpublished)
                .collect();
            recover_partial_stage(transaction, retained, members)
        }
    }
}

fn recover_partial_stage(
    transaction: TransactionState,
    retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
    members: Vec<TransientPublicationMember>,
) -> ManagedContentTransactionOutcome {
    let remaining = members
        .into_iter()
        .zip(retained)
        .enumerate()
        .map(|(index, (member, (id, report, authority)))| match member {
            TransientPublicationMember::Published(file) => StageRecoveryMember::Published {
                index,
                id,
                report,
                authority,
                file,
            },
            TransientPublicationMember::Unpublished(stage) => {
                StageRecoveryMember::Unpublished(VerifiedCreateOnly::from_content_stage(
                    stage, report, authority,
                ))
            }
        })
        .collect();
    drive_stage_cleanup(transaction, remaining)
}

enum StageRecoveryMember {
    Published {
        index: usize,
        id: ManagedContentPayloadId,
        report: TransferReport,
        authority: ManagedTransferAuthority,
        file: FileCapability,
    },
    Unpublished(VerifiedCreateOnly),
}

fn drive_stage_cleanup(
    mut transaction: TransactionState,
    remaining: Vec<StageRecoveryMember>,
) -> ManagedContentTransactionOutcome {
    let mut remaining = remaining.into_iter();
    while let Some(member) = remaining.next() {
        match member {
            StageRecoveryMember::Published {
                index,
                id,
                report,
                authority,
                file,
            } => {
                let name = PortableFileName::new_exact(&format!("payload-{index}"))
                    .expect("bounded payload index is portable");
                let guard = match content_guard_from_file(
                    &transaction.stage,
                    LeafName::new(name.as_str()).expect("payload name is a native leaf"),
                    file,
                ) {
                    Ok(guard) => guard,
                    Err((_error, file)) => {
                        let mut retained = vec![StageRecoveryMember::Published {
                            index,
                            id,
                            report,
                            authority,
                            file,
                        }];
                        retained.extend(remaining);
                        return ManagedContentTransactionOutcome::RecoveryRequired(
                            ManagedContentRecovery {
                                state: Some(RecoveryState::StageFilePending {
                                    transaction,
                                    remaining: retained,
                                }),
                            },
                        );
                    }
                };
                transaction
                    .staged_by_id
                    .insert(id.clone(), transaction.payloads.len());
                transaction.payloads.push(StagedPayload {
                    name,
                    report,
                    guard: Some(guard),
                });
                drop(authority);
            }
            StageRecoveryMember::Unpublished(verified) => match verified.discard() {
                VerifiedTransferDiscardOutcome::Discarded { .. } => {}
                VerifiedTransferDiscardOutcome::Pending(obligation) => {
                    return ManagedContentTransactionOutcome::RecoveryRequired(
                        ManagedContentRecovery {
                            state: Some(RecoveryState::StageDiscardPending {
                                transaction,
                                obligation: Some(obligation),
                                remaining: remaining.collect(),
                            }),
                        },
                    );
                }
            },
        }
    }
    drive_rollback(transaction, false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn content_root(
        temporary: &tempfile::TempDir,
    ) -> (super::super::ManagedTreeRoot, ManagedContentTransactionRoot) {
        let path = temporary.path();
        for child in ["mods", "resourcepacks", "shaderpacks"] {
            std::fs::create_dir_all(path.join(child)).expect("content parent");
        }
        let tree = super::super::ManagedTreeRoot::open_for_test(path).expect("managed tree");
        let operation = tree.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("tree directory");
        let root = ManagedContentTransactionRoot::bind(
            directory,
            ManagedTransferAuthority::retain(Arc::new(())),
        );
        (tree, root)
    }

    fn absent_plan(
        session: &ManagedContentTransactionSession,
        path: PortableRelativePath,
    ) -> ManagedContentMutationPlan {
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("encoded manifest");
        ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path,
                ManagedContentObservedState::Absent,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect("content plan")
    }

    fn transaction_session(
        root: ManagedContentTransactionRoot,
        paths: Vec<PortableRelativePath>,
    ) -> ManagedContentTransactionSession {
        let planning = root.observe_manifest().expect("manifest observation");
        let planning = planning
            .observe_more(paths.clone())
            .expect("path observation");
        planning.finish(paths).expect("transaction observation")
    }

    fn prepared(
        session: ManagedContentTransactionSession,
        plan: ManagedContentMutationPlan,
    ) -> ManagedContentPreparedTransaction {
        match session.prepare(plan) {
            ManagedContentPreparationOutcome::Prepared(prepared) => prepared,
            _ => panic!("content preparation must succeed"),
        }
    }

    #[test]
    fn plan_rejects_duplicate_payload_use_and_reserved_names() {
        let path = PortableRelativePath::new_exact("mods/example.jar").expect("path");
        let observation = ManagedContentPathObservation {
            path: path.clone(),
            state: ManagedContentObservedState::Absent,
        };
        let payload = ManagedContentPayloadId::new("payload").expect("payload id");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(1).expect("nonzero"),
            crate::download::ExpectedTransferDigests::sha512([0_u8; 64]),
        )
        .expect("authenticated contract");
        let plan = ManagedContentMutationPlan::new(
            &[observation],
            vec![ManagedContentPathMutation::new(
                path,
                ManagedContentObservedState::Absent,
                ManagedContentPathResult::Download(payload.clone()),
            )],
            vec![ManagedContentPayloadPlan::new(payload, contract)],
            ManagedContentEncodedManifest {
                body: Box::from(&b"{}"[..]),
                session: Arc::new(()),
            },
        );
        assert!(plan.is_ok());

        let reserved = PortableRelativePath::new_exact("mods/axial.content.json")
            .expect("portable reserved path");
        assert_eq!(
            validate_content_path(&reserved),
            Err(ManagedContentPlanError::ReservedName)
        );
    }

    #[test]
    fn observation_and_plan_share_one_exact_transaction_budget() {
        let mut remaining = MAX_CONTENT_TRANSACTION_BYTES;
        assert!(admit_observed_bytes(&mut remaining, MAX_CONTENT_TRANSACTION_BYTES).is_ok());
        assert_eq!(remaining, 0);
        assert!(matches!(
            admit_observed_bytes(&mut remaining, 1),
            Err(FileObservationFailure::TransactionBudgetExceeded)
        ));

        let payload = ManagedContentPayloadId::new("replacement").expect("payload");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(1).expect("nonzero"),
            crate::download::ExpectedTransferDigests::sha512([0_u8; 64]),
        )
        .expect("contract");
        let mut observations = Vec::new();
        let mut mutations = Vec::new();
        for index in 0..4 {
            let path = PortableRelativePath::new_exact(&format!("mods/budget-{index}.jar"))
                .expect("path");
            let observed = ManagedContentObservedState::Exact {
                size: MAX_CONTENT_FILE_BYTES,
                sha512: "00".repeat(64).into_boxed_str(),
            };
            observations.push(ManagedContentPathObservation {
                path: path.clone(),
                state: observed.clone(),
            });
            mutations.push(ManagedContentPathMutation::new(
                path,
                observed,
                if index == 0 {
                    ManagedContentPathResult::Download(payload.clone())
                } else {
                    ManagedContentPathResult::Absent
                },
            ));
        }
        assert!(matches!(
            ManagedContentMutationPlan::new(
                &observations,
                mutations,
                vec![ManagedContentPayloadPlan::new(payload, contract)],
                ManagedContentEncodedManifest {
                    body: Box::from(&b"{}"[..]),
                    session: Arc::new(()),
                },
            ),
            Err(ManagedContentPlanError::TransactionBudgetExceeded)
        ));
    }

    #[test]
    fn manifest_first_planning_is_incremental_and_selects_one_inspected_subset() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join(MANIFEST_NAME), b"manifest").expect("manifest");
        std::fs::write(temporary.path().join("mods/first.jar"), b"first").expect("first");
        std::fs::write(temporary.path().join("mods/second.jar"), b"second").expect("second");
        let (_tree, root) = content_root(&temporary);
        let first = PortableRelativePath::new_exact("mods/first.jar").expect("first path");
        let second = PortableRelativePath::new_exact("mods/second.jar").expect("second path");

        let planning = root.observe_manifest().expect("manifest observation");
        assert_eq!(planning.manifest_bytes(), Some(&b"manifest"[..]));
        assert!(matches!(
            planning.manifest_state(),
            ManagedContentObservedState::Exact { size: 8, .. }
        ));
        let planning = planning
            .observe_more(vec![first.clone()])
            .expect("first observation");
        let planning = planning
            .observe_more(vec![second.clone()])
            .expect("second observation");
        assert_eq!(planning.observations().len(), 2);

        let failure = planning
            .observe_more(vec![second.clone()])
            .expect_err("duplicate cumulative path must fail");
        assert_eq!(failure.error(), ManagedContentObservationError::DuplicatePath);
        let planning = failure.into_session();
        assert_eq!(planning.observations().len(), 2);
        let alias = PortableRelativePath::new_exact("mods/SECOND.jar").expect("alias path");
        let failure = planning
            .finish(vec![alias])
            .expect_err("portable alias must not replace the inspected spelling");
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::MissingObservation
        );
        let planning = failure.into_session();
        let session = planning
            .finish(vec![second.clone()])
            .expect("selected transaction subset");
        let observations = session.observations();
        assert_eq!(observations.len(), 1);
        assert_eq!(observations[0].path(), &second);
        let alias = PortableRelativePath::new_exact("mods/SECOND.jar").expect("alias path");
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("bound manifest");
        let plan_error = ManagedContentMutationPlan::new(
            &observations,
            vec![ManagedContentPathMutation::new(
                alias,
                observations[0].state().clone(),
                ManagedContentPathResult::Preserve,
            )],
            Vec::new(),
            manifest,
        )
        .expect_err("portable alias must not replace the selected spelling");
        assert_eq!(plan_error, ManagedContentPlanError::MissingObservation);
    }

    #[test]
    fn failed_manifest_observation_returns_the_no_effect_root() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::write(
            temporary.path().join(MANIFEST_NAME),
            vec![0_u8; MAX_MANIFEST_BYTES + 1],
        )
        .expect("oversized manifest");
        let (_tree, root) = content_root(&temporary);
        let failure = root
            .observe_manifest()
            .expect_err("oversized manifest must fail");
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::ManifestTooLarge
        );
        let root = failure.into_root();
        std::fs::remove_file(temporary.path().join(MANIFEST_NAME)).expect("remove manifest");
        let planning = root.observe_manifest().expect("absent manifest observation");
        assert_eq!(planning.manifest_state(), &ManagedContentObservedState::Absent);
        assert_eq!(planning.manifest_bytes(), None);
    }

    #[test]
    fn plan_from_an_aliased_session_cannot_bind_a_later_exact_guard() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let lower = PortableRelativePath::new_exact("mods/dependency.jar").expect("lower path");
        let upper = PortableRelativePath::new_exact("mods/DEPENDENCY.jar").expect("upper path");
        let (tree, root) = content_root(&temporary);
        std::fs::write(lower.join_under(temporary.path()), b"dependency").expect("dependency");
        let earlier = transaction_session(root, vec![lower.clone()]);
        let earlier_observations = earlier.observations();
        drop(earlier);
        drop(tree);
        std::fs::remove_file(lower.join_under(temporary.path())).expect("remove earlier spelling");
        std::fs::write(upper.join_under(temporary.path()), b"dependency")
            .expect("write aliased replacement");

        let (_tree, root) = content_root(&temporary);
        let later = transaction_session(root, vec![upper.clone()]);
        let manifest = later
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("later manifest");
        let plan = ManagedContentMutationPlan::new(
            &earlier_observations,
            vec![ManagedContentPathMutation::new(
                lower,
                earlier_observations[0].state().clone(),
                ManagedContentPathResult::Preserve,
            )],
            Vec::new(),
            manifest,
        )
        .expect("earlier exact plan");
        let returned = match later.prepare(plan) {
            ManagedContentPreparationOutcome::Refused { error, session } => {
                assert_eq!(
                    error,
                    ManagedContentPreparationError::PlanDoesNotMatchObservation
                );
                session
            }
            _ => panic!("aliased earlier plan must be refused before effects"),
        };
        assert_eq!(returned.observations()[0].path(), &upper);
        assert!(
            std::fs::read_dir(temporary.path())
                .expect("instance entries")
                .all(|entry| !entry
                    .expect("instance entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-content-"))
        );
    }

    #[test]
    fn late_batch_failure_retains_successful_observations_and_budget() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/first.jar"), b"first").expect("first");
        let (_tree, root) = content_root(&temporary);
        std::fs::remove_dir(temporary.path().join("resourcepacks"))
            .expect("remove unavailable parent");
        let first = PortableRelativePath::new_exact("mods/first.jar").expect("first path");
        let unavailable = PortableRelativePath::new_exact("resourcepacks/later.zip")
            .expect("unavailable path");

        let planning = root.observe_manifest().expect("manifest observation");
        let failure = planning
            .observe_more(vec![first.clone(), unavailable])
            .expect_err("missing second parent must fail after the first observation");
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::ParentUnavailable
        );
        let planning = failure.into_session();
        assert_eq!(planning.observations().len(), 1);
        assert_eq!(planning.observations()[0].path(), &first);
        let failure = planning
            .observe_more(vec![first])
            .expect_err("successful prefix must remain cumulatively observed");
        assert_eq!(failure.error(), ManagedContentObservationError::DuplicatePath);
    }

    #[test]
    fn prepared_cancel_removes_its_reserved_namespace() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/cancelled.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = absent_plan(&session, path);
        let outcome = prepared(session, plan).cancel();
        assert!(matches!(outcome, ManagedContentTransactionOutcome::Cancelled(_)));
        assert!(
            std::fs::read_dir(temporary.path())
                .expect("instance entries")
                .all(|entry| !entry
                    .expect("instance entry")
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".axial-content-"))
        );
    }

    #[test]
    fn uninstall_commit_removes_observed_file_and_publishes_manifest() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/remove.jar"), b"old")
            .expect("old content");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/remove.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path,
                observed,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect("uninstall plan");
        let (awaiting, slots) = prepared(session, plan).into_transfer_slots();
        assert!(slots.is_empty());
        let ready = match awaiting.accept_verified(Vec::new()) {
            ManagedContentStageOutcome::Ready(ready) => ready,
            _ => panic!("empty staging must be ready"),
        };
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Committed(_)
        ));
        assert!(!temporary.path().join("mods/remove.jar").exists());
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).expect("manifest"),
            b"{}"
        );
    }

    #[test]
    fn preserved_dependency_drift_is_rejected_before_the_first_effect() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        std::fs::write(temporary.path().join("mods/dependency.jar"), b"dependency")
            .expect("dependency");
        let path = PortableRelativePath::new_exact("mods/dependency.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path,
                observed,
                ManagedContentPathResult::Preserve,
            )],
            Vec::new(),
            manifest,
        )
        .expect("preserve plan");
        let (awaiting, slots) = prepared(session, plan).into_transfer_slots();
        assert!(slots.is_empty());
        std::fs::write(temporary.path().join("mods/dependency.jar"), b"drifted")
            .expect("drift dependency");
        let ready = match awaiting.accept_verified(Vec::new()) {
            ManagedContentStageOutcome::Ready(ready) => ready,
            _ => panic!("empty staging must be ready"),
        };
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
        assert_eq!(
            std::fs::read(temporary.path().join("mods/dependency.jar")).expect("dependency"),
            b"drifted"
        );
    }

    #[test]
    fn preserved_dependency_is_never_claimed_or_removed() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/dependency.jar"), b"dependency")
            .expect("dependency");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/dependency.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path,
                observed,
                ManagedContentPathResult::Preserve,
            )],
            Vec::new(),
            manifest,
        )
        .expect("preserve plan");
        let (awaiting, slots) = prepared(session, plan).into_transfer_slots();
        assert!(slots.is_empty());
        let ready = match awaiting.accept_verified(Vec::new()) {
            ManagedContentStageOutcome::Ready(ready) => ready,
            _ => panic!("empty staging must be ready"),
        };
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Committed(_)
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("mods/dependency.jar")).expect("dependency"),
            b"dependency"
        );
    }

    #[test]
    fn drift_before_commit_rolls_back_without_touching_foreign_file() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/foreign.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = absent_plan(&session, path);
        let (awaiting, slots) = prepared(session, plan).into_transfer_slots();
        assert!(slots.is_empty());
        std::fs::write(temporary.path().join("mods/foreign.jar"), b"foreign")
            .expect("foreign content");
        let ready = match awaiting.accept_verified(Vec::new()) {
            ManagedContentStageOutcome::Ready(ready) => ready,
            _ => panic!("empty staging must be ready"),
        };
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("mods/foreign.jar")).expect("foreign content"),
            b"foreign"
        );
    }
}
