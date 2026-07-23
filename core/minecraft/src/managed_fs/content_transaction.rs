use super::{
    ManagedCreateOnlyWriteFailure, ManagedDir, ManagedExactChildCleanup, ManagedFileGuard,
    ManagedTreeDirectory, MAX_MANAGED_DIRECTORY_ENTRIES, hex_lower,
};
use crate::download::{
    CreateOnlyTransferTarget, ManagedTransferAuthority, ManagedTransferTerminalAuthority,
    RetryPolicy, TransferByteContract, TransferCancellation, TransferCleanupObligation,
    TransferCleanupResolution, TransferClient, TransferContract, TransferFailureReport,
    TransferOutcome, TransferReport, TransferTargetCancelObligation, TransferTargetCancelOutcome,
    TransferTask, TransferUnsettledObligation, VerifiedCreateOnly,
    VerifiedTransferDiscardObligation, VerifiedTransferDiscardOutcome, start_create_only_transfer,
};
use crate::loaders::LoaderError;
use crate::portable_path::{
    PortableFileName, PortablePathKey, PortableRelativePath, managed_content_name_is_reserved,
    managed_content_name_key,
};
use axial_fs::{
    FileCapability, LeafName, TransientPublicationBatch, TransientPublicationBatchObligation,
    TransientPublicationBatchOutcome, TransientPublicationMember,
};
use sha2::{Digest as _, Sha512};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};
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
    remaining_transaction_bytes: u64,
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
        let mut payload_bytes = 0_u64;
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
            payload_bytes = payload_bytes
                .checked_add(limit)
                .ok_or(ManagedContentPlanError::TransactionBudgetExceeded)?;
            if aggregate_bytes > MAX_CONTENT_TRANSACTION_BYTES {
                return Err(ManagedContentPlanError::TransactionBudgetExceeded);
            }
        }
        if payload_bytes > manifest.remaining_transaction_bytes {
            return Err(ManagedContentPlanError::TransactionBudgetExceeded);
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

/// Opaque proof that Content data came from one exact Core planning session.
/// It is intentionally move-only and exposes no serializable identity.
#[must_use = "content planning bindings must remain with their decoded projection"]
pub struct ManagedContentPlanningBinding {
    session: Arc<()>,
}

impl fmt::Debug for ManagedContentPlanningBinding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentPlanningBinding")
            .finish_non_exhaustive()
    }
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

    pub fn planning_binding(&self) -> ManagedContentPlanningBinding {
        ManagedContentPlanningBinding {
            session: Arc::clone(&self.manifest_session),
        }
    }

    pub fn matches_planning_binding(&self, binding: &ManagedContentPlanningBinding) -> bool {
        Arc::ptr_eq(&self.manifest_session, &binding.session)
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
    read_preconditions: Vec<PathObservationAuthority>,
    remaining_transaction_bytes: u64,
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
    pub fn manifest_state(&self) -> &ManagedContentObservedState {
        &self.manifest.state
    }

    pub fn manifest_bytes(&self) -> Option<&[u8]> {
        self.manifest.bytes.as_deref()
    }

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
            remaining_transaction_bytes: self.remaining_transaction_bytes,
        })
    }

    pub fn observations(&self) -> Vec<ManagedContentPathObservation> {
        self.observations
            .iter()
            .map(|observation| observation.public.clone())
            .collect()
    }

    pub fn matches_planning_binding(&self, binding: &ManagedContentPlanningBinding) -> bool {
        Arc::ptr_eq(&self.manifest_session, &binding.session)
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

    let mut logical_names = HashMap::<String, Vec<PortableFileName>>::new();
    for path in &paths {
        let (parent_name, name) = split_content_path(path);
        logical_names
            .entry(parent_name.to_string())
            .or_default()
            .push(name);
    }

    let mut parents = HashMap::<String, ManagedDir>::new();
    for path in paths {
        let key = path.key();
        let exact_path = path.clone();
        let (parent_name, name) = split_content_path(&path);
        if !parents.contains_key(parent_name) {
            let parent = match session.root.open_child(parent_name) {
                Ok(parent) => parent,
                Err(_) => {
                    return Err(refuse(
                        ManagedContentObservationError::ParentUnavailable,
                        session,
                    ));
                }
            };
            let names = logical_names
                .get(parent_name)
                .expect("validated batch retains its logical parent names");
            if validate_managed_logical_name_bindings(
                names.iter().map(|name| (&parent, name)),
            )
            .is_err()
            {
                return Err(refuse(
                    ManagedContentObservationError::NonPortableEntry,
                    session,
                ));
            }
            parents.insert(parent_name.to_string(), parent);
        }
        let parent = parents
            .get(parent_name)
            .expect("opened managed content parent")
            .clone();
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
    if validate_managed_logical_name_bindings(
        session
            .observations
            .iter()
            .map(|observation| (&observation.parent, &observation.name)),
    )
    .is_err()
    {
        return Err(ManagedContentPlanningObservationFailure {
            error: ManagedContentObservationError::NonPortableEntry,
            session,
        });
    }
    let ManagedContentPlanningSession {
        root,
        authority,
        manifest,
        manifest_session,
        observations,
        observed_paths: _,
        remaining_bytes,
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
    let read_preconditions = observations_by_key.into_values().collect();
    Ok(ManagedContentTransactionSession {
        root,
        authority,
        manifest,
        observations,
        read_preconditions,
        remaining_transaction_bytes: remaining_bytes,
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

#[derive(Clone)]
struct ManagedLogicalNameSpec {
    key: PortablePathKey,
    enabled: PortableFileName,
    disabled: PortableFileName,
}

fn managed_logical_name_spec(
    name: &PortableFileName,
) -> Result<ManagedLogicalNameSpec, FileObservationFailure> {
    let key = managed_content_name_key(name);
    let enabled = if name.key() == key {
        name.clone()
    } else {
        let enabled = name
            .as_str()
            .strip_suffix(".disabled")
            .and_then(|value| PortableFileName::new_exact(value).ok())
            .ok_or(FileObservationFailure::NonPortableEntry)?;
        if managed_content_name_key(&enabled) != enabled.key() {
            return Err(FileObservationFailure::NonPortableEntry);
        }
        enabled
    };
    let disabled = enabled
        .with_suffix(".disabled")
        .map_err(|_| FileObservationFailure::NonPortableEntry)?;
    if name != &enabled && name != &disabled {
        return Err(FileObservationFailure::NonPortableEntry);
    }
    Ok(ManagedLogicalNameSpec {
        key,
        enabled,
        disabled,
    })
}

fn validate_managed_logical_name_bindings<'a>(
    bindings: impl Iterator<Item = (&'a ManagedDir, &'a PortableFileName)>,
) -> Result<(), FileObservationFailure> {
    let mut groups = HashMap::new();
    for (parent, name) in bindings {
        let spec = managed_logical_name_spec(name)?;
        let (_, watched) = groups
            .entry(parent.inner.identity)
            .or_insert_with(|| (parent, BTreeMap::<PortablePathKey, ManagedLogicalNameSpec>::new()));
        match watched.get(&spec.key) {
            Some(existing)
                if existing.enabled != spec.enabled || existing.disabled != spec.disabled =>
            {
                return Err(FileObservationFailure::NonPortableEntry);
            }
            Some(_) => {}
            None => {
                watched.insert(spec.key.clone(), spec);
            }
        }
    }
    for (_, (parent, watched)) in groups {
        let entries = parent
            .entries_bounded(MAX_MANAGED_DIRECTORY_ENTRIES)
            .map_err(|_| FileObservationFailure::Unavailable)?;
        for entry in entries {
            let raw = entry
                .to_str()
                .ok_or(FileObservationFailure::NonPortableEntry)?;
            let name = PortableFileName::new_exact(raw)
                .map_err(|_| FileObservationFailure::NonPortableEntry)?;
            let key = managed_content_name_key(&name);
            let Some(spec) = watched.get(&key) else {
                continue;
            };
            if name != spec.enabled && name != spec.disabled {
                return Err(FileObservationFailure::NonPortableEntry);
            }
        }
    }
    Ok(())
}

struct ManagedContentTransferGroup {
    _state_authority: ManagedTransferAuthority,
}

struct ManagedContentTransferSlot {
    id: ManagedContentPayloadId,
    contract: TransferContract,
    target: CreateOnlyTransferTarget,
    cancellation: ManagedContentSlotCancellation,
}

struct ManagedContentSlotCancellation {
    id: ManagedContentPayloadId,
    authority: ManagedTransferAuthority,
}

struct ManagedContentVerifiedTransfer {
    cancellation: ManagedContentSlotCancellation,
    verified: VerifiedCreateOnly,
}

/// Core-owned sequential transfer state. Only one exact slot can be in flight.
#[must_use = "content transfer batches must issue, stage, or cancel every exact slot"]
pub struct ManagedContentTransferBatch {
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
    remaining: VecDeque<ManagedContentTransferSlot>,
    payload_count: usize,
}

impl fmt::Debug for ManagedContentTransferBatch {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransferBatch")
            .field("verified", &self.verified.len())
            .field("remaining", &self.remaining.len())
            .finish_non_exhaustive()
    }
}

#[must_use = "the next content transfer step must be completed or cancelled"]
pub enum ManagedContentTransferStep {
    Issued(ManagedContentIssuedTransfer),
    Complete(ManagedContentCompleteTransfers),
}

impl fmt::Debug for ManagedContentTransferStep {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Issued(_) => "Issued",
            Self::Complete(_) => "Complete",
        };
        formatter
            .debug_struct("ManagedContentTransferStep")
            .field("variant", &variant)
            .finish()
    }
}

/// One exact admitted destination with its transaction continuation retained.
#[must_use = "issued content transfers must start or cancel"]
pub struct ManagedContentIssuedTransfer {
    slot: ManagedContentTransferSlot,
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
    remaining: VecDeque<ManagedContentTransferSlot>,
    payload_count: usize,
}

impl fmt::Debug for ManagedContentIssuedTransfer {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentIssuedTransfer")
            .field("id", &self.slot.id)
            .finish_non_exhaustive()
    }
}

struct ManagedContentTransferContinuation {
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
    cancellation: ManagedContentSlotCancellation,
    remaining: VecDeque<ManagedContentTransferSlot>,
    payload_count: usize,
}

/// Joined transfer owner whose outcome is already bound to its exact slot.
#[must_use = "content transfer tasks must be joined before advancing the transaction"]
pub struct ManagedContentTransferTask {
    task: TransferTask<VerifiedCreateOnly>,
    continuation: ManagedContentTransferContinuation,
}

impl fmt::Debug for ManagedContentTransferTask {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransferTask")
            .finish_non_exhaustive()
    }
}

/// A joined outcome that still owns the exact transaction continuation.
#[must_use = "joined content transfers must advance or unwind the transaction"]
pub struct ManagedContentTransferSettlement {
    continuation: ManagedContentTransferContinuation,
    outcome: TransferOutcome<VerifiedCreateOnly>,
}

impl fmt::Debug for ManagedContentTransferSettlement {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransferSettlement")
            .field("outcome", &self.outcome)
            .finish_non_exhaustive()
    }
}

#[must_use = "content transfer advancement retains the complete transaction"]
pub enum ManagedContentTransferAdvance {
    Continue(ManagedContentTransferBatch),
    Unwind(ManagedContentTransactionOutcome),
}

impl fmt::Debug for ManagedContentTransferAdvance {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Continue(_) => "Continue",
            Self::Unwind(_) => "Unwind",
        };
        formatter
            .debug_struct("ManagedContentTransferAdvance")
            .field("variant", &variant)
            .finish()
    }
}

/// Complete exact verified set, still cancellable before private publication.
#[must_use = "complete content transfers must stage or cancel"]
pub struct ManagedContentCompleteTransfers {
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
}

impl fmt::Debug for ManagedContentCompleteTransfers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentCompleteTransfers")
            .field("payloads", &self.verified.len())
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransferBatch {
    pub fn payload_count(&self) -> usize {
        self.payload_count
    }

    pub fn next(mut self) -> ManagedContentTransferStep {
        match self.remaining.pop_front() {
            Some(slot) => ManagedContentTransferStep::Issued(ManagedContentIssuedTransfer {
                slot,
                state: self.state,
                verified: self.verified,
                remaining: self.remaining,
                payload_count: self.payload_count,
            }),
            None => ManagedContentTransferStep::Complete(ManagedContentCompleteTransfers {
                state: self.state,
                verified: self.verified,
            }),
        }
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        cancel_transfer_batch(self.state, self.verified, self.remaining)
    }
}

impl ManagedContentIssuedTransfer {
    pub fn id(&self) -> &ManagedContentPayloadId {
        &self.slot.id
    }

    pub fn start(
        self,
        client: TransferClient,
        url: reqwest::Url,
        retry: RetryPolicy,
        cancellation: TransferCancellation,
    ) -> ManagedContentTransferTask {
        let Self {
            slot,
            state,
            verified,
            remaining,
            payload_count,
        } = self;
        let ManagedContentTransferSlot {
            id: _,
            contract,
            target,
            cancellation: slot_cancellation,
        } = slot;
        ManagedContentTransferTask {
            task: start_create_only_transfer(
                client,
                url,
                target,
                contract,
                retry,
                cancellation,
            ),
            continuation: ManagedContentTransferContinuation {
                state,
                verified,
                cancellation: slot_cancellation,
                remaining,
                payload_count,
            },
        }
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        let Self {
            slot,
            state,
            verified,
            mut remaining,
            payload_count: _,
        } = self;
        remaining.push_front(slot);
        cancel_transfer_batch(state, verified, remaining)
    }
}

impl ManagedContentTransferTask {
    pub fn cancel(&self) {
        self.task.cancel();
    }

    pub async fn join(self) -> ManagedContentTransferSettlement {
        let Self { task, continuation } = self;
        ManagedContentTransferSettlement {
            continuation,
            outcome: task.join().await,
        }
    }
}

impl ManagedContentTransferSettlement {
    pub fn failure_report(&self) -> Option<&TransferFailureReport> {
        match &self.outcome {
            TransferOutcome::Complete(_) => None,
            TransferOutcome::Failed { report, .. } => Some(report),
            TransferOutcome::CleanupPending(obligation) => Some(obligation.report()),
            TransferOutcome::Unsettled(obligation) => Some(obligation.report()),
        }
    }

    pub fn is_complete(&self) -> bool {
        matches!(&self.outcome, TransferOutcome::Complete(_))
    }

    pub fn advance(self) -> ManagedContentTransferAdvance {
        advance_transfer_settlement(self)
    }
}

impl ManagedContentCompleteTransfers {
    pub fn stage(self) -> ManagedContentStageOutcome {
        accept_verified_transfers(self.state, self.verified)
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        cancel_transfer_batch(self.state, self.verified, VecDeque::new())
    }
}

struct ManagedContentCancelledSlot {
    _id: ManagedContentPayloadId,
    _authority: ManagedTransferTerminalAuthority,
}

enum SlotAuthorityAdmission {
    Admitted(ManagedContentCancelledSlot),
    Refused {
        cancellation: ManagedContentSlotCancellation,
        authority: ManagedTransferTerminalAuthority,
    },
}

fn admit_slot_authority(
    cancellation: ManagedContentSlotCancellation,
    authority: ManagedTransferTerminalAuthority,
) -> SlotAuthorityAdmission {
    if !authority.shares_retained_authority(&cancellation.authority) {
        return SlotAuthorityAdmission::Refused {
            cancellation,
            authority,
        };
    }
    SlotAuthorityAdmission::Admitted(ManagedContentCancelledSlot {
        _id: cancellation.id,
        _authority: authority,
    })
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
    authority: ManagedTransferAuthority,
    private_name: PortableFileName,
    private: ManagedDir,
    stage: ManagedDir,
    backup: ManagedDir,
    manifest: ExactObservation,
    manifest_body: Box<[u8]>,
    mutations: Vec<TransactionMutation>,
    read_preconditions: Vec<PathObservationAuthority>,
    planned_payloads: Vec<PlannedPayload>,
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
    #[cfg(test)]
    before_manifest_revalidation: Option<Box<dyn FnOnce() + Send>>,
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
            slots.push(ManagedContentTransferSlot {
                id: id.clone(),
                contract: payload.contract.clone(),
                target: CreateOnlyTransferTarget::new(
                    destination,
                    group_authority.retained(),
                ),
                cancellation: ManagedContentSlotCancellation {
                    id: id.clone(),
                    authority: group_authority.retained(),
                },
            });
            planned_payloads.push(PlannedPayload {
                id,
                contract: payload.contract.clone(),
                authority: group_authority.retained(),
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
            authority: group_authority,
            private_name,
            private,
            stage,
            backup,
            manifest: session.manifest,
            manifest_body: plan.manifest.body,
            mutations,
            read_preconditions: session.read_preconditions,
            planned_payloads,
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
            #[cfg(test)]
            before_manifest_revalidation: None,
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

#[must_use = "verified content stages must become ready or remain retained"]
pub enum ManagedContentStageOutcome {
    Ready(ManagedContentReadyTransaction),
    Unwind(ManagedContentTransactionOutcome),
}

impl fmt::Debug for ManagedContentStageOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Ready(_) => "Ready",
            Self::Unwind(_) => "Unwind",
        };
        formatter
            .debug_struct("ManagedContentStageOutcome")
            .field("variant", &variant)
            .finish()
    }
}

impl ManagedContentPreparedTransaction {
    pub fn into_transfer_batch(self) -> ManagedContentTransferBatch {
        let Self { state, slots } = self;
        let payload_count = slots.len();
        ManagedContentTransferBatch {
            state,
            verified: Vec::with_capacity(payload_count),
            remaining: slots.into(),
            payload_count,
        }
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        self.into_transfer_batch().cancel()
    }
}

fn advance_transfer_settlement(
    settlement: ManagedContentTransferSettlement,
) -> ManagedContentTransferAdvance {
    let ManagedContentTransferSettlement {
        continuation,
        outcome,
    } = settlement;
    let ManagedContentTransferContinuation {
        state,
        mut verified,
        cancellation,
        remaining,
        payload_count,
    } = continuation;
    let planned = &state.planned_payloads[verified.len()];
    let authority_matches = transfer_outcome_shares_authority(&outcome, &planned.authority);
    let contract_matches = match &outcome {
        TransferOutcome::Complete(value) => {
            report_matches_contract(value.report(), &planned.contract)
        }
        _ => true,
    };
    if authority_matches && contract_matches {
        if let TransferOutcome::Complete(value) = outcome {
            verified.push(ManagedContentVerifiedTransfer {
                cancellation,
                verified: value,
            });
            return ManagedContentTransferAdvance::Continue(ManagedContentTransferBatch {
                state,
                verified,
                remaining,
                payload_count,
            });
        }
    }

    let mut members = verified
        .into_iter()
        .map(verified_transfer_unwind_member)
        .collect::<Vec<_>>();
    members.push(TransferUnwindMember::from_parts(cancellation, outcome));
    members.extend(remaining.into_iter().map(TransferUnwindMember::Unstarted));
    ManagedContentTransferAdvance::Unwind(drive_transfer_unwind(state, members))
}

fn cancel_transfer_batch(
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
    remaining: VecDeque<ManagedContentTransferSlot>,
) -> ManagedContentTransactionOutcome {
    let members = verified
        .into_iter()
        .map(verified_transfer_unwind_member)
        .chain(remaining.into_iter().map(TransferUnwindMember::Unstarted))
        .collect();
    drive_transfer_unwind(state, members)
}

fn verified_transfer_unwind_member(
    transfer: ManagedContentVerifiedTransfer,
) -> TransferUnwindMember {
    TransferUnwindMember::Verified {
        cancellation: transfer.cancellation,
        verified: transfer.verified,
    }
}

fn accept_verified_transfers(
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
) -> ManagedContentStageOutcome {
    debug_assert_eq!(verified.len(), state.planned_payloads.len());
    let mut stages = Vec::with_capacity(verified.len());
    let mut retained = Vec::with_capacity(verified.len());
    let mut cancellations = Vec::with_capacity(verified.len());
    for (planned, transfer) in state.planned_payloads.iter().zip(verified) {
        let (stage, report, authority) = transfer.verified.into_content_stage();
        stages.push(stage);
        retained.push((planned.id.clone(), report, authority));
        cancellations.push(transfer.cancellation);
    }
    if stages.is_empty() {
        return ManagedContentStageOutcome::Ready(ManagedContentReadyTransaction { state });
    }
    let batch = match TransientPublicationBatch::new(stages) {
        Ok(batch) => batch,
        Err(failure) => {
            let verified = failure
                .into_stages()
                .into_iter()
                .zip(retained.into_iter().zip(cancellations))
                .map(|(stage, ((_, report, authority), cancellation))| {
                    ManagedContentVerifiedTransfer {
                        cancellation,
                        verified: VerifiedCreateOnly::from_content_stage(stage, report, authority),
                    }
                })
                .collect();
            return ManagedContentStageOutcome::Unwind(cancel_transfer_batch(
                state,
                verified,
                VecDeque::new(),
            ));
        }
    };
    map_stage_publication(state, retained, cancellations, batch.publish_create_new())
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

fn transfer_outcome_shares_authority(
    outcome: &TransferOutcome<VerifiedCreateOnly>,
    authority: &ManagedTransferAuthority,
) -> bool {
    match outcome {
        TransferOutcome::Complete(verified) => verified.shares_retained_authority(authority),
        TransferOutcome::Failed {
            authority: terminal,
            ..
        } => terminal.shares_retained_authority(authority),
        TransferOutcome::CleanupPending(obligation) => {
            obligation.shares_retained_authority(authority)
        }
        TransferOutcome::Unsettled(obligation) => {
            obligation.shares_retained_authority(authority)
        }
    }
}

enum TransferUnwindMember {
    Verified {
        cancellation: ManagedContentSlotCancellation,
        verified: VerifiedCreateOnly,
    },
    CleanupPending {
        cancellation: ManagedContentSlotCancellation,
        obligation: TransferCleanupObligation,
    },
    Unsettled {
        cancellation: ManagedContentSlotCancellation,
        obligation: TransferUnsettledObligation,
    },
    Unstarted(ManagedContentTransferSlot),
    VerifiedDiscardPending {
        cancellation: ManagedContentSlotCancellation,
        obligation: VerifiedTransferDiscardObligation,
    },
    TargetCancelPending {
        cancellation: ManagedContentSlotCancellation,
        obligation: TransferTargetCancelObligation,
    },
    Terminal {
        cancellation: ManagedContentSlotCancellation,
        authority: ManagedTransferTerminalAuthority,
    },
}

impl TransferUnwindMember {
    fn from_parts(
        cancellation: ManagedContentSlotCancellation,
        outcome: TransferOutcome<VerifiedCreateOnly>,
    ) -> Self {
        match outcome {
            TransferOutcome::Complete(verified) => Self::Verified {
                cancellation,
                verified,
            },
            TransferOutcome::Failed {
                report: _,
                authority,
            } => Self::Terminal {
                cancellation,
                authority,
            },
            TransferOutcome::CleanupPending(obligation) => Self::CleanupPending {
                cancellation,
                obligation,
            },
            TransferOutcome::Unsettled(obligation) => Self::Unsettled {
                cancellation,
                obligation,
            },
        }
    }
}

enum TransferUnwindAdvance {
    Settled,
    Retained(TransferUnwindMember),
}

fn drive_transfer_unwind(
    state: TransactionState,
    members: Vec<TransferUnwindMember>,
) -> ManagedContentTransactionOutcome {
    let mut retained = Vec::with_capacity(members.len());
    let mut unsettled = Vec::new();
    for member in members {
        if matches!(&member, TransferUnwindMember::Unsettled { .. }) {
            unsettled.push(member);
        } else if let TransferUnwindAdvance::Retained(member) = advance_transfer_unwind(member) {
            retained.push(member);
        }
    }
    if unsettled.is_empty() {
        // No abandoned transfer effect needs the stronger root-settlement witness.
    } else if let Ok(settlement) = state.root.settle_transfer_effects(&state.authority) {
        for member in unsettled {
            let TransferUnwindMember::Unsettled {
                cancellation,
                obligation,
            } = member
            else {
                unreachable!("unsettled transfer partition retains only unsettled members")
            };
            match obligation.reconcile_after_effect_settlement(&settlement) {
                Ok((_report, authority)) => {
                    if let TransferUnwindAdvance::Retained(member) =
                        admit_terminal_unwind(cancellation, authority)
                    {
                        retained.push(member);
                    }
                }
                Err(obligation) => retained.push(TransferUnwindMember::Unsettled {
                    cancellation,
                    obligation,
                }),
            }
        }
    } else {
        retained.extend(unsettled);
    }
    if retained.is_empty() {
        drive_rollback(state, true)
    } else {
        ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
            state: Some(RecoveryState::TransferUnwind {
                transaction: state,
                members: retained,
            }),
        })
    }
}

fn advance_transfer_unwind(member: TransferUnwindMember) -> TransferUnwindAdvance {
    match member {
        TransferUnwindMember::Verified {
            cancellation,
            verified,
        } => match verified.discard() {
            VerifiedTransferDiscardOutcome::Discarded { authority, .. } => {
                admit_terminal_unwind(cancellation, authority)
            }
            VerifiedTransferDiscardOutcome::Pending(obligation) => {
                TransferUnwindAdvance::Retained(TransferUnwindMember::VerifiedDiscardPending {
                    cancellation,
                    obligation,
                })
            }
        },
        TransferUnwindMember::CleanupPending {
            cancellation,
            obligation,
        } => match obligation.reconcile() {
            TransferCleanupResolution::Discarded { authority, .. } => {
                admit_terminal_unwind(cancellation, authority)
            }
            TransferCleanupResolution::Pending(obligation) => {
                TransferUnwindAdvance::Retained(TransferUnwindMember::CleanupPending {
                    cancellation,
                    obligation,
                })
            }
        },
        TransferUnwindMember::Unsettled {
            cancellation,
            obligation,
        } => TransferUnwindAdvance::Retained(TransferUnwindMember::Unsettled {
            cancellation,
            obligation,
        }),
        TransferUnwindMember::Unstarted(slot) => {
            let ManagedContentTransferSlot {
                id: _,
                contract: _,
                target,
                cancellation,
            } = slot;
            match target.cancel() {
                TransferTargetCancelOutcome::Cancelled(authority) => {
                    admit_terminal_unwind(cancellation, authority)
                }
                TransferTargetCancelOutcome::Pending(obligation) => {
                    TransferUnwindAdvance::Retained(TransferUnwindMember::TargetCancelPending {
                        cancellation,
                        obligation,
                    })
                }
            }
        }
        TransferUnwindMember::VerifiedDiscardPending {
            cancellation,
            obligation,
        } => match obligation.reconcile() {
            VerifiedTransferDiscardOutcome::Discarded { authority, .. } => {
                admit_terminal_unwind(cancellation, authority)
            }
            VerifiedTransferDiscardOutcome::Pending(obligation) => {
                TransferUnwindAdvance::Retained(TransferUnwindMember::VerifiedDiscardPending {
                    cancellation,
                    obligation,
                })
            }
        },
        TransferUnwindMember::TargetCancelPending {
            cancellation,
            obligation,
        } => match obligation.reconcile() {
            TransferTargetCancelOutcome::Cancelled(authority) => {
                admit_terminal_unwind(cancellation, authority)
            }
            TransferTargetCancelOutcome::Pending(obligation) => {
                TransferUnwindAdvance::Retained(TransferUnwindMember::TargetCancelPending {
                    cancellation,
                    obligation,
                })
            }
        },
        TransferUnwindMember::Terminal {
            cancellation,
            authority,
        } => admit_terminal_unwind(cancellation, authority),
    }
}

fn admit_terminal_unwind(
    cancellation: ManagedContentSlotCancellation,
    authority: ManagedTransferTerminalAuthority,
) -> TransferUnwindAdvance {
    match admit_slot_authority(cancellation, authority) {
        SlotAuthorityAdmission::Admitted(receipt) => {
            drop(receipt);
            TransferUnwindAdvance::Settled
        }
        SlotAuthorityAdmission::Refused {
            cancellation,
            authority,
        } => TransferUnwindAdvance::Retained(TransferUnwindMember::Terminal {
            cancellation,
            authority,
        }),
    }
}

fn map_stage_publication(
    mut state: TransactionState,
    retained: Vec<(ManagedContentPayloadId, TransferReport, ManagedTransferAuthority)>,
    cancellations: Vec<ManagedContentSlotCancellation>,
    outcome: TransientPublicationBatchOutcome,
) -> ManagedContentStageOutcome {
    match outcome {
        TransientPublicationBatchOutcome::Published(files) => {
            drop(cancellations);
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
                        return ManagedContentStageOutcome::Unwind(
                            ManagedContentTransactionOutcome::RecoveryRequired(
                                ManagedContentRecovery {
                                    state: Some(RecoveryState::StageFilePending {
                                        transaction: state,
                                        remaining,
                                    }),
                                },
                            ),
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
                .zip(retained.into_iter().zip(cancellations))
                .map(|(stage, ((_, report, authority), cancellation))| {
                    ManagedContentVerifiedTransfer {
                        cancellation,
                        verified: VerifiedCreateOnly::from_content_stage(stage, report, authority),
                    }
                })
                .collect();
            ManagedContentStageOutcome::Unwind(cancel_transfer_batch(
                state,
                verified,
                VecDeque::new(),
            ))
        }
        TransientPublicationBatchOutcome::Partial { members, .. } => {
            drop(cancellations);
            ManagedContentStageOutcome::Unwind(
                ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
                    state: Some(RecoveryState::StagePartial {
                        transaction: state,
                        retained,
                        members,
                    }),
                }),
            )
        }
        TransientPublicationBatchOutcome::Pending(obligation) => {
            drop(cancellations);
            ManagedContentStageOutcome::Unwind(
                ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
                    state: Some(RecoveryState::StagePending {
                        transaction: state,
                        retained,
                        obligation: Some(obligation),
                    }),
                }),
            )
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
        if state.mutations[index].old_guard.is_none() {
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
        if (mutation.claimed || mutation.installed)
            && synced.insert(mutation.parent.inner.identity)
            && mutation.parent.sync().is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::SyncFailed;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    #[cfg(test)]
    if let Some(hook) = state.before_manifest_revalidation.take() {
        hook();
    }
    if !revalidate_transaction_logical_names(&state)
        || !revalidate_read_preconditions(&state)
        || !revalidate_final_effects(&state)
    {
        state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
        return drive_rollback(state, false);
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
        && revalidate_transaction_logical_names(state)
        && state.mutations.iter().all(|mutation| {
            observed_binding_matches(&mutation.parent, mutation.name.as_str(), &mutation.old_guard)
        })
        && revalidate_read_preconditions(state)
}

fn revalidate_transaction_logical_names(state: &TransactionState) -> bool {
    validate_managed_logical_name_bindings(
        state
            .mutations
            .iter()
            .map(|mutation| (&mutation.parent, &mutation.name))
            .chain(
                state
                    .read_preconditions
                    .iter()
                    .map(|observation| (&observation.parent, &observation.name)),
            ),
    )
    .is_ok()
}

fn observed_binding_matches(
    parent: &ManagedDir,
    name: &str,
    guard: &Option<ManagedFileGuard>,
) -> bool {
    match guard.as_ref() {
        Some(guard) => classify_exact_file(parent, name, guard) == ExactBindingState::Exact,
        None => classify_name(parent, name) == ExactBindingState::Absent,
    }
}

fn revalidate_read_preconditions(state: &TransactionState) -> bool {
    state.read_preconditions.iter().all(|precondition| {
        observed_binding_matches(
            &precondition.parent,
            precondition.name.as_str(),
            &precondition.guard,
        )
    })
}

fn revalidate_final_effects(state: &TransactionState) -> bool {
    state.mutations.iter().all(|mutation| match &mutation.result {
        ManagedContentPathResult::Absent => {
            classify_name(&mutation.parent, mutation.name.as_str()) == ExactBindingState::Absent
        }
        ManagedContentPathResult::Download(_) => {
            mutation.installed_guard.as_ref().is_some_and(|guard| {
                classify_exact_file(&mutation.parent, mutation.name.as_str(), guard)
                    == ExactBindingState::Exact
            })
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
    mut state: TransactionState,
    intent: TransactionIntent,
) -> ManagedContentTransactionOutcome {
    state.read_preconditions.clear();
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
    TransferUnwind {
        transaction: TransactionState,
        members: Vec<TransferUnwindMember>,
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
            RecoveryState::TransferUnwind {
                transaction,
                members,
            } => drive_transfer_unwind(transaction, members),
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

    fn retained_content_root(
        tree: &super::super::ManagedTreeRoot,
    ) -> ManagedContentTransactionRoot {
        let operation = tree.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("tree directory");
        ManagedContentTransactionRoot::bind(
            directory,
            ManagedTransferAuthority::retain(Arc::new(())),
        )
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

    fn download_plan(
        session: &ManagedContentTransactionSession,
    ) -> ManagedContentMutationPlan {
        let observations = session.observations();
        let mut mutations = Vec::with_capacity(observations.len());
        let mut payloads = Vec::with_capacity(observations.len());
        for (index, observation) in observations.iter().enumerate() {
            let id = ManagedContentPayloadId::new(&format!("payload-{index}"))
                .expect("payload id");
            let contract = TransferContract::authenticated_exact(
                std::num::NonZeroU64::new(1).expect("nonzero"),
                crate::download::ExpectedTransferDigests::sha512([index as u8; 64]),
            )
            .expect("transfer contract");
            mutations.push(ManagedContentPathMutation::new(
                observation.path().clone(),
                observation.state().clone(),
                ManagedContentPathResult::Download(id.clone()),
            ));
            payloads.push(ManagedContentPayloadPlan::new(id, contract));
        }
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("encoded manifest");
        ManagedContentMutationPlan::new(&observations, mutations, payloads, manifest)
            .expect("download plan")
    }

    fn transaction_session(
        root: ManagedContentTransactionRoot,
        paths: Vec<PortableRelativePath>,
    ) -> ManagedContentTransactionSession {
        transaction_session_with_effects(root, paths.clone(), paths)
    }

    fn transaction_session_with_effects(
        root: ManagedContentTransactionRoot,
        observed_paths: Vec<PortableRelativePath>,
        effect_paths: Vec<PortableRelativePath>,
    ) -> ManagedContentTransactionSession {
        let planning = root.observe_manifest().expect("manifest observation");
        let planning = planning
            .observe_more(observed_paths)
            .expect("path observation");
        planning
            .finish(effect_paths)
            .expect("transaction observation")
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

    fn ready_without_transfers(
        prepared: ManagedContentPreparedTransaction,
    ) -> ManagedContentReadyTransaction {
        let complete = match prepared.into_transfer_batch().next() {
            ManagedContentTransferStep::Complete(complete) => complete,
            ManagedContentTransferStep::Issued(_) => {
                panic!("transaction unexpectedly retained a transfer")
            }
        };
        match complete.stage() {
            ManagedContentStageOutcome::Ready(ready) => ready,
            ManagedContentStageOutcome::Unwind(_) => {
                panic!("empty transfer staging unexpectedly unwound")
            }
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
                remaining_transaction_bytes: MAX_CONTENT_TRANSACTION_BYTES,
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
                    remaining_transaction_bytes: MAX_CONTENT_TRANSACTION_BYTES,
                },
            ),
            Err(ManagedContentPlanError::TransactionBudgetExceeded)
        ));

        let path = PortableRelativePath::new_exact("mods/remaining-budget.jar").expect("path");
        let payload = ManagedContentPayloadId::new("remaining-budget").expect("payload");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(1).expect("nonzero"),
            crate::download::ExpectedTransferDigests::sha512([0_u8; 64]),
        )
        .expect("contract");
        assert!(matches!(
            ManagedContentMutationPlan::new(
                &[ManagedContentPathObservation {
                    path: path.clone(),
                    state: ManagedContentObservedState::Absent,
                }],
                vec![ManagedContentPathMutation::new(
                    path,
                    ManagedContentObservedState::Absent,
                    ManagedContentPathResult::Download(payload.clone()),
                )],
                vec![ManagedContentPayloadPlan::new(payload, contract)],
                ManagedContentEncodedManifest {
                    body: Box::from(&b"{}"[..]),
                    session: Arc::new(()),
                    remaining_transaction_bytes: 0,
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
        assert_eq!(session.manifest_bytes(), Some(&b"manifest"[..]));
        assert!(matches!(
            session.manifest_state(),
            ManagedContentObservedState::Exact { size: 8, .. }
        ));
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
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect_err("portable alias must not replace the selected spelling");
        assert_eq!(plan_error, ManagedContentPlanError::MissingObservation);
    }

    #[test]
    fn planning_binding_matches_only_its_exact_planning_flow() {
        let first = tempfile::tempdir().expect("first instance");
        let second = tempfile::tempdir().expect("second instance");
        let (_first_tree, first_root) = content_root(&first);
        let (_second_tree, second_root) = content_root(&second);
        let first_planning = first_root.observe_manifest().expect("first planning");
        let second_planning = second_root.observe_manifest().expect("second planning");
        let binding = first_planning.planning_binding();

        assert!(first_planning.matches_planning_binding(&binding));
        assert!(!second_planning.matches_planning_binding(&binding));
        let first_session = first_planning.finish(Vec::new()).expect("first session");
        assert!(first_session.matches_planning_binding(&binding));
    }

    #[test]
    fn managed_logical_name_observation_rejects_arbitrary_disabled_aliases() {
        for (fixture, alias) in [
            ("repeated-disabled", "example.jar.disabled.disabled"),
            ("case-alias", "EXAMPLE.JAR"),
            ("disabled-case-alias", "example.jar.DISABLED"),
        ] {
            let temporary = tempfile::tempdir().expect("temporary instance");
            std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
            std::fs::write(temporary.path().join("mods").join(alias), fixture.as_bytes())
                .expect("alias");
            let (_tree, root) = content_root(&temporary);
            let path = PortableRelativePath::new_exact("mods/example.jar").expect("path");
            let failure = root
                .observe_manifest()
                .expect("manifest observation")
                .observe_more(vec![path])
                .expect_err("managed logical alias must fail closed");
            assert_eq!(
                failure.error(),
                ManagedContentObservationError::NonPortableEntry
            );
        }
    }

    #[test]
    fn exact_enabled_and_disabled_variants_are_allowed_but_late_aliases_are_not() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/example.jar"), b"enabled")
            .expect("enabled");
        std::fs::write(
            temporary.path().join("mods/example.jar.disabled"),
            b"disabled",
        )
        .expect("disabled");
        let (_tree, root) = content_root(&temporary);
        let enabled = PortableRelativePath::new_exact("mods/example.jar").expect("enabled path");
        let disabled = PortableRelativePath::new_exact("mods/example.jar.disabled")
            .expect("disabled path");
        let planning = root
            .observe_manifest()
            .expect("manifest observation")
            .observe_more(vec![enabled, disabled])
            .expect("two exact variants remain observable");
        std::fs::write(
            temporary.path().join("mods/example.jar.disabled.disabled"),
            b"late alias",
        )
        .expect("late alias");
        let failure = planning
            .finish(Vec::new())
            .expect_err("late managed logical alias must fail closed");
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::NonPortableEntry
        );
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
                ManagedContentPathResult::Absent,
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
    fn transfer_batch_issues_only_the_next_exact_slot() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let paths = vec![
            PortableRelativePath::new_exact("mods/first.jar").expect("first path"),
            PortableRelativePath::new_exact("mods/second.jar").expect("second path"),
        ];
        let session = transaction_session(root, paths);
        let plan = download_plan(&session);
        let batch = prepared(session, plan).into_transfer_batch();
        assert_eq!(batch.payload_count(), 2);
        let issued = match batch.next() {
            ManagedContentTransferStep::Issued(issued) => issued,
            ManagedContentTransferStep::Complete(_) => panic!("first slot must be issued"),
        };
        assert_eq!(issued.id().as_str(), "payload-0");
        assert!(matches!(
            issued.cancel(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
    }

    #[test]
    fn complete_unstarted_batch_drives_transaction_cancellation() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let paths = vec![
            PortableRelativePath::new_exact("mods/first.jar").expect("first path"),
            PortableRelativePath::new_exact("mods/second.jar").expect("second path"),
        ];
        let session = transaction_session(root, paths);
        let plan = download_plan(&session);
        assert!(matches!(
            prepared(session, plan).into_transfer_batch().cancel(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
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
    fn unsettled_slot_progresses_after_the_exact_root_can_settle() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (tree, root) = content_root(&temporary);
        let paths = vec![
            PortableRelativePath::new_exact("mods/first.jar").expect("first path"),
            PortableRelativePath::new_exact("mods/second.jar").expect("second path"),
        ];
        let session = transaction_session(root, paths);
        let plan = download_plan(&session);
        let issued = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Issued(issued) => issued,
            ManagedContentTransferStep::Complete(_) => panic!("first slot must be issued"),
        };
        let ManagedContentIssuedTransfer {
            slot,
            state,
            verified,
            remaining,
            payload_count,
        } = issued;
        let ManagedContentTransferSlot {
            id: _,
            contract: _,
            target,
            cancellation,
        } = slot;
        let _terminal = match target.cancel() {
            TransferTargetCancelOutcome::Cancelled(authority) => authority,
            TransferTargetCancelOutcome::Pending(_) => panic!("target cancellation pending"),
        };
        let unsettled_authority = cancellation.authority.retained();
        let settlement = ManagedContentTransferSettlement {
            continuation: ManagedContentTransferContinuation {
                state,
                verified,
                cancellation,
                remaining,
                payload_count,
            },
            outcome: TransferOutcome::Unsettled(TransferUnsettledObligation::for_test(
                TransferFailureReport::for_test(
                    crate::download::TransferFailureKind::WorkerStopped,
                ),
                unsettled_authority,
            )),
        };

        let blocker_root = retained_content_root(&tree);
        let blocker_path =
            PortableRelativePath::new_exact("mods/blocker.jar").expect("blocker path");
        let blocker_session = transaction_session(blocker_root, vec![blocker_path]);
        let blocker_plan = download_plan(&blocker_session);
        let blocker = prepared(blocker_session, blocker_plan);

        let recovery = match settlement.advance() {
            ManagedContentTransferAdvance::Unwind(
                ManagedContentTransactionOutcome::RecoveryRequired(recovery),
            ) => recovery,
            _ => panic!("live sibling slot must retain unsettled root recovery"),
        };
        match recovery.state.as_ref().expect("retained transfer recovery") {
            RecoveryState::TransferUnwind { members, .. } => {
                assert_eq!(members.len(), 1);
                assert!(members
                    .iter()
                    .all(|member| matches!(member, TransferUnwindMember::Unsettled { .. })));
            }
            _ => panic!("unsettled slot must retain exact transfer recovery"),
        }
        assert!(matches!(
            blocker.cancel(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
        assert!(matches!(
            recovery.reconcile(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
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
        let ready = ready_without_transfers(prepared(session, plan));
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
    fn manifest_only_transaction_publishes_without_pseudo_mutations() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let enabled = PortableRelativePath::new_exact("mods/missing.jar").expect("enabled path");
        let disabled =
            PortableRelativePath::new_exact("mods/missing.jar.disabled").expect("disabled path");
        let session = transaction_session_with_effects(root, vec![enabled, disabled], Vec::new());
        assert!(session.observations().is_empty());
        assert_eq!(session.read_preconditions.len(), 2);
        let manifest = session
            .bind_encoded_manifest(b"{\"entries\":[]}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(&[], Vec::new(), Vec::new(), manifest)
            .expect("manifest-only plan");
        let prepared = prepared(session, plan);
        assert!(prepared.state.mutations.is_empty());
        assert_eq!(prepared.state.read_preconditions.len(), 2);
        let ready = ready_without_transfers(prepared);
        let receipt = match ready.commit() {
            ManagedContentTransactionOutcome::Committed(receipt) => receipt,
            _ => panic!("manifest-only transaction must commit"),
        };
        assert_eq!(receipt.path_count(), 0);
        assert_eq!(receipt.payload_count(), 0);
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).expect("manifest"),
            b"{\"entries\":[]}"
        );
    }

    #[test]
    fn more_than_effect_limit_read_preconditions_remain_non_effects() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/effect.jar").expect("effect path");
        let mut observed_paths = vec![effect.clone()];
        observed_paths.extend((0..=MAX_CONTENT_PATHS).map(|index| {
            PortableRelativePath::new_exact(&format!("mods/precondition-{index}.jar"))
                .expect("precondition path")
        }));
        let session =
            transaction_session_with_effects(root, observed_paths, vec![effect.clone()]);
        assert_eq!(session.observations().len(), 1);
        assert_eq!(session.read_preconditions.len(), MAX_CONTENT_PATHS + 1);
        let plan = absent_plan(&session, effect);
        let prepared = prepared(session, plan);
        assert_eq!(prepared.state.mutations.len(), 1);
        assert_eq!(
            prepared.state.read_preconditions.len(),
            MAX_CONTENT_PATHS + 1
        );
        let ready = ready_without_transfers(prepared);
        let receipt = match ready.commit() {
            ManagedContentTransactionOutcome::Committed(receipt) => receipt,
            _ => panic!("bounded effect transaction must commit"),
        };
        assert_eq!(receipt.path_count(), 1);
        assert_eq!(receipt.payload_count(), 0);
    }

    #[test]
    fn read_precondition_drift_is_rejected_before_the_first_effect() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/remove.jar").expect("effect path");
        let dependency =
            PortableRelativePath::new_exact("mods/dependency.jar").expect("dependency path");
        std::fs::write(effect.join_under(temporary.path()), b"old").expect("old effect");
        std::fs::write(dependency.join_under(temporary.path()), b"dependency")
            .expect("dependency");
        let session = transaction_session_with_effects(
            root,
            vec![effect.clone(), dependency.clone()],
            vec![effect.clone()],
        );
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                effect.clone(),
                observed,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect("removal plan");
        std::fs::write(dependency.join_under(temporary.path()), b"drifted")
            .expect("drift dependency");
        let ready = ready_without_transfers(prepared(session, plan));
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert_eq!(
            std::fs::read(effect.join_under(temporary.path())).expect("old effect"),
            b"old"
        );
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
    }

    #[test]
    fn read_precondition_drift_after_an_effect_rolls_back_before_manifest() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/remove.jar").expect("effect path");
        let dependency =
            PortableRelativePath::new_exact("mods/dependency.jar").expect("dependency path");
        let effect_path = effect.join_under(temporary.path());
        let dependency_path = dependency.join_under(temporary.path());
        std::fs::write(&effect_path, b"old").expect("old effect");
        std::fs::write(&dependency_path, b"dependency").expect("dependency");
        let session = transaction_session_with_effects(
            root,
            vec![effect.clone(), dependency],
            vec![effect.clone()],
        );
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                effect,
                observed,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect("removal plan");
        let mut ready = ready_without_transfers(prepared(session, plan));
        ready.state.before_manifest_revalidation = Some(Box::new(move || {
            std::fs::write(dependency_path, b"drifted").expect("drift dependency");
        }));
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert_eq!(std::fs::read(effect_path).expect("restored effect"), b"old");
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
    }

    #[test]
    fn final_effect_drift_blocks_manifest_and_recovery_ignores_read_preconditions() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/remove.jar").expect("effect path");
        let dependency =
            PortableRelativePath::new_exact("mods/dependency.jar").expect("dependency path");
        let effect_path = effect.join_under(temporary.path());
        let dependency_path = dependency.join_under(temporary.path());
        std::fs::write(&effect_path, b"old").expect("old effect");
        std::fs::write(&dependency_path, b"dependency").expect("dependency");
        let session = transaction_session_with_effects(
            root,
            vec![effect.clone(), dependency],
            vec![effect.clone()],
        );
        let observed = session.observations()[0].state().clone();
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                effect,
                observed,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            manifest,
        )
        .expect("removal plan");
        let mut ready = ready_without_transfers(prepared(session, plan));
        let effect_drift_path = effect_path.clone();
        ready.state.before_manifest_revalidation = Some(Box::new(move || {
            std::fs::write(effect_drift_path, b"foreign").expect("drift final effect");
        }));
        let recovery = match ready.commit() {
            ManagedContentTransactionOutcome::RecoveryRequired(recovery) => recovery,
            _ => panic!("foreign final effect must retain rollback recovery"),
        };
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
        std::fs::write(&dependency_path, b"drifted").expect("drift read precondition");
        std::fs::remove_file(&effect_path).expect("remove foreign effect");
        assert!(matches!(
            recovery.reconcile(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ClaimFailed
            )
        ));
        assert_eq!(std::fs::read(effect_path).expect("restored effect"), b"old");
        assert_eq!(
            std::fs::read(dependency_path).expect("drifted dependency"),
            b"drifted"
        );
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
    }

    #[test]
    fn drift_before_commit_rolls_back_without_touching_foreign_file() {
        let temporary = tempfile::tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/foreign.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = absent_plan(&session, path);
        std::fs::write(temporary.path().join("mods/foreign.jar"), b"foreign")
            .expect("foreign content");
        let ready = ready_without_transfers(prepared(session, plan));
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
