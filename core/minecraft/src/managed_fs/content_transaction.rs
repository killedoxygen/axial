use super::{
    MAX_MANAGED_DIRECTORY_ENTRIES, ManagedCreateOnlyWriteFailure, ManagedDir,
    ManagedExactChildCleanup, ManagedFileGuard, ManagedTreeDirectory, hex_lower,
};
use crate::download::{
    CreateOnlyTransferTarget, ManagedTransferAuthority, ManagedTransferTerminalAuthority,
    RetryPolicy, TransferByteContract, TransferCancellation, TransferCleanupObligation,
    TransferCleanupResolution, TransferClient, TransferContract, TransferFailureReport,
    TransferOutcome, TransferReport, TransferTargetCancelObligation, TransferTargetCancelOutcome,
    TransferTask, TransferUnsettledObligation, VerifiedCreateOnly,
    VerifiedTransferDiscardObligation, VerifiedTransferDiscardOutcome, copy_create_only_transfer,
    fail_create_only_transfer, start_create_only_transfer,
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
use serde::{Deserialize, Serialize};
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
// An 8 MiB Modrinth index cannot encode this many valid file records, even
// before the separately bounded 10,000-file override tree is included.
const MAX_PACK_CONTENT_PATHS: usize = 100_000;
const MAX_CONTENT_FILE_BYTES: u64 = 1 << 30;
const MAX_CONTENT_TRANSACTION_BYTES: u64 = 4 << 30;
#[cfg(target_os = "linux")]
const MAX_TRANSIENT_STAGE_MEMBERS: usize = 512;
// Named portable stages retain one native stage effect and one transient
// reservation each. Keep their combined use inside the existing 512-effect
// root budget; anonymous Linux stages still use one slot per member.
#[cfg(not(target_os = "linux"))]
const MAX_TRANSIENT_STAGE_MEMBERS: usize = 256;
const MAX_CONTENT_PRIVATE_DIRECTORIES: usize = 16;
const PRIVATE_STAGE_NAME: &str = "stage";
const PRIVATE_BACKUP_NAME: &str = "backup";
const MAX_STAGING_CHECKPOINT_BYTES: usize = 16 * 1024 * 1024;

#[cfg(any(test, feature = "test-support"))]
type BeforeManifestRevalidation = Box<dyn FnOnce() + Send>;

#[cfg(any(test, feature = "test-support"))]
static BEFORE_MANIFEST_REVALIDATION: std::sync::OnceLock<
    std::sync::Mutex<HashMap<std::path::PathBuf, BeforeManifestRevalidation>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
thread_local! {
    static AFTER_STAGING_PARENT_REMOVAL: std::cell::Cell<Option<fn()>> = const { std::cell::Cell::new(None) };
    static AFTER_PUBLISHED_FILE_REMOVAL: std::cell::Cell<Option<fn()>> = const { std::cell::Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManagedContentPathPolicy {
    Managed,
    Pack,
}

impl ManagedContentPathPolicy {
    fn final_path_limit(self) -> usize {
        match self {
            Self::Managed => MAX_CONTENT_PATHS,
            Self::Pack => MAX_PACK_CONTENT_PATHS,
        }
    }

    fn planning_path_limit(self) -> usize {
        match self {
            Self::Managed => MAX_CONTENT_PLANNING_PATHS,
            Self::Pack => MAX_PACK_CONTENT_PATHS,
        }
    }

    fn transaction_byte_limit(self) -> u64 {
        match self {
            Self::Managed => MAX_CONTENT_TRANSACTION_BYTES,
            // The prior pack path had per-file and override bounds but no
            // aggregate indexed-download byte ceiling.
            Self::Pack => u64::MAX,
        }
    }
}

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
    source: ManagedContentPayloadSourcePlan,
}

#[derive(Clone, Debug)]
enum ManagedContentPayloadSourcePlan {
    Remote,
    Observation(PortableRelativePath),
    External,
}

impl ManagedContentPayloadPlan {
    pub fn new(id: ManagedContentPayloadId, contract: TransferContract) -> Self {
        Self {
            id,
            contract,
            source: ManagedContentPayloadSourcePlan::Remote,
        }
    }

    pub fn from_observation(
        id: ManagedContentPayloadId,
        contract: TransferContract,
        source: PortableRelativePath,
    ) -> Self {
        Self {
            id,
            contract,
            source: ManagedContentPayloadSourcePlan::Observation(source),
        }
    }

    pub fn from_external_source(id: ManagedContentPayloadId, contract: TransferContract) -> Self {
        Self {
            id,
            contract,
            source: ManagedContentPayloadSourcePlan::External,
        }
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
    path_policy: ManagedContentPathPolicy,
}

#[must_use = "deferred manifests remain bound to the observing content session"]
pub struct ManagedContentDeferredManifest {
    session: Arc<()>,
    remaining_transaction_bytes: u64,
    path_policy: ManagedContentPathPolicy,
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
    manifest: ManagedContentManifestPlan,
    remaining_manifest_bytes: u64,
}

enum ManagedContentManifestPlan {
    Encoded(ManagedContentEncodedManifest),
    Deferred(ManagedContentDeferredManifest),
}

impl ManagedContentManifestPlan {
    fn session(&self) -> &Arc<()> {
        match self {
            Self::Encoded(manifest) => &manifest.session,
            Self::Deferred(manifest) => &manifest.session,
        }
    }

    fn remaining_transaction_bytes(&self) -> u64 {
        match self {
            Self::Encoded(manifest) => manifest.remaining_transaction_bytes,
            Self::Deferred(manifest) => manifest.remaining_transaction_bytes,
        }
    }

    fn path_policy(&self) -> ManagedContentPathPolicy {
        match self {
            Self::Encoded(manifest) => manifest.path_policy,
            Self::Deferred(manifest) => manifest.path_policy,
        }
    }

    fn into_parts(self) -> (Box<[u8]>, u64) {
        match self {
            Self::Encoded(manifest) => (manifest.body, manifest.remaining_transaction_bytes),
            Self::Deferred(manifest) => (Box::default(), manifest.remaining_transaction_bytes),
        }
    }
}

impl fmt::Debug for ManagedContentMutationPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let manifest_bytes = match &self.manifest {
            ManagedContentManifestPlan::Encoded(manifest) => Some(manifest.body.len()),
            ManagedContentManifestPlan::Deferred(_) => None,
        };
        formatter
            .debug_struct("ManagedContentMutationPlan")
            .field("paths", &self.mutations.len())
            .field("payloads", &self.payloads.len())
            .field("manifest_bytes", &manifest_bytes)
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
        Self::validated(
            observations,
            mutations,
            payloads,
            ManagedContentManifestPlan::Encoded(manifest),
        )
    }

    pub fn new_deferred(
        observations: &[ManagedContentPathObservation],
        mutations: Vec<ManagedContentPathMutation>,
        payloads: Vec<ManagedContentPayloadPlan>,
        manifest: ManagedContentDeferredManifest,
    ) -> Result<Self, ManagedContentPlanError> {
        Self::validated(
            observations,
            mutations,
            payloads,
            ManagedContentManifestPlan::Deferred(manifest),
        )
    }

    fn validated(
        observations: &[ManagedContentPathObservation],
        mutations: Vec<ManagedContentPathMutation>,
        payloads: Vec<ManagedContentPayloadPlan>,
        manifest: ManagedContentManifestPlan,
    ) -> Result<Self, ManagedContentPlanError> {
        let path_limit = manifest.path_policy().final_path_limit();
        let transaction_byte_limit = manifest.path_policy().transaction_byte_limit();
        if mutations.len() > path_limit
            || observations.len() > path_limit
            || payloads.len() > path_limit
        {
            return Err(ManagedContentPlanError::TooManyPaths);
        }
        let mut observed_by_path = BTreeMap::new();
        let mut aggregate_bytes = 0_u64;
        for observation in observations {
            validate_content_path(manifest.path_policy(), &observation.path)?;
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
                if aggregate_bytes > transaction_byte_limit {
                    return Err(ManagedContentPlanError::TransactionBudgetExceeded);
                }
            }
        }

        let mut payload_ids = BTreeSet::new();
        let mut local_sources = BTreeMap::new();
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
            if aggregate_bytes > transaction_byte_limit {
                return Err(ManagedContentPlanError::TransactionBudgetExceeded);
            }
            if let ManagedContentPayloadSourcePlan::Observation(source) = &payload.source {
                validate_content_path(manifest.path_policy(), source)?;
                let Some(observation) = observed_by_path.get(&source.key()) else {
                    return Err(ManagedContentPlanError::MissingObservation);
                };
                if observation.path != *source
                    || !contract_matches_observation(&payload.contract, &observation.state)
                {
                    return Err(ManagedContentPlanError::ObservationChanged);
                }
                if local_sources
                    .insert(source.key(), payload.id.clone())
                    .is_some()
                {
                    return Err(ManagedContentPlanError::DuplicatePayloadUse);
                }
            }
        }
        if payload_bytes > manifest.remaining_transaction_bytes() {
            return Err(ManagedContentPlanError::TransactionBudgetExceeded);
        }
        let mut mutation_paths = BTreeSet::new();
        let mut used_payloads = BTreeSet::new();
        for mutation in &mutations {
            validate_content_path(manifest.path_policy(), &mutation.path)?;
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
        for source in local_sources.into_keys() {
            if !mutations.iter().any(|mutation| {
                mutation.path.key() == source
                    && matches!(mutation.result, ManagedContentPathResult::Absent)
            }) {
                return Err(ManagedContentPlanError::ObservationChanged);
            }
        }
        let remaining_manifest_bytes = manifest
            .remaining_transaction_bytes()
            .saturating_sub(payload_bytes);
        Ok(Self {
            mutations,
            payloads,
            manifest,
            remaining_manifest_bytes,
        })
    }
}

fn contract_matches_observation(
    contract: &TransferContract,
    observation: &ManagedContentObservedState,
) -> bool {
    let ManagedContentObservedState::Exact { size, sha512 } = observation else {
        return false;
    };
    let size_matches = match std::num::NonZeroU64::new(*size) {
        Some(size) => contract.bytes() == TransferByteContract::Exact(size),
        None => {
            contract.bytes()
                == TransferByteContract::Below(
                    std::num::NonZeroU64::new(1).expect("one is nonzero"),
                )
        }
    };
    size_matches
        && contract
            .digests()
            .expected_sha512()
            .is_some_and(|expected| hex_lower(expected) == sha512.as_ref())
}

fn transfer_contract_limit(contract: &TransferContract) -> u64 {
    match contract.bytes() {
        TransferByteContract::Exact(value)
        | TransferByteContract::AtMost(value)
        | TransferByteContract::Below(value) => value.get(),
    }
}

fn validate_content_path(
    policy: ManagedContentPathPolicy,
    path: &PortableRelativePath,
) -> Result<(), ManagedContentPlanError> {
    if policy == ManagedContentPathPolicy::Pack {
        let components = path.as_str().split('/').collect::<Vec<_>>();
        if components.is_empty() || components.len() > 64 {
            return Err(ManagedContentPlanError::InvalidPath);
        }
        let first = PortableFileName::new_exact(components[0])
            .map_err(|_| ManagedContentPlanError::InvalidPath)?;
        let managed_parent =
            ["mods", "resourcepacks", "shaderpacks"]
                .into_iter()
                .find(|candidate| {
                    first.key()
                        == PortableFileName::new_exact(candidate)
                            .expect("managed parent is portable")
                            .key()
                });
        if managed_parent.is_some_and(|candidate| candidate != first.as_str()) {
            return Err(ManagedContentPlanError::InvalidPath);
        }
        let name = path.file_name();
        if managed_content_name_is_reserved(&name)
            && (components.len() == 1 || managed_parent.is_some())
        {
            return Err(ManagedContentPlanError::ReservedName);
        }
        return Ok(());
    }
    let mut segments = path.as_str().split('/');
    let Some(parent) = segments.next() else {
        return Err(ManagedContentPlanError::InvalidPath);
    };
    let Some(name) = segments.next() else {
        return Err(ManagedContentPlanError::InvalidPath);
    };
    if segments.next().is_some() || !matches!(parent, "mods" | "resourcepacks" | "shaderpacks") {
        return Err(ManagedContentPlanError::InvalidPath);
    }
    let name =
        PortableFileName::new_exact(name).map_err(|_| ManagedContentPlanError::InvalidPath)?;
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
    parent: TransactionParent,
    name: PortableFileName,
    guard: Option<ManagedFileGuard>,
}

#[derive(Clone)]
enum TransactionParent {
    Resolved {
        directory: ManagedDir,
    },
    Missing {
        path: PortableRelativePath,
        first_missing: usize,
    },
}

impl TransactionParent {
    fn directory(&self) -> Option<&ManagedDir> {
        match self {
            Self::Resolved { directory, .. } => Some(directory),
            Self::Missing { .. } => None,
        }
    }

    fn resolved(&self) -> &ManagedDir {
        self.directory()
            .expect("transaction effects require a materialized parent")
    }
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
    path_policy: ManagedContentPathPolicy,
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
    path_policy: ManagedContentPathPolicy,
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
            path_policy: self.path_policy,
        })
    }

    pub fn defer_manifest(&self) -> ManagedContentDeferredManifest {
        ManagedContentDeferredManifest {
            session: Arc::clone(&self.manifest_session),
            remaining_transaction_bytes: self.remaining_transaction_bytes,
            path_policy: self.path_policy,
        }
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

    pub fn prepare(self, plan: ManagedContentMutationPlan) -> ManagedContentPreparationOutcome {
        prepare_transaction(self, plan)
    }
}

#[must_use = "managed content transaction authority must be retained through settlement"]
pub struct ManagedContentTransactionRoot {
    directory: ManagedTreeDirectory,
    authority: ManagedTransferAuthority,
    path_policy: ManagedContentPathPolicy,
}

impl fmt::Debug for ManagedContentTransactionRoot {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentTransactionRoot")
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransactionRoot {
    /// Arms only the next transaction prepared for this canonical fixture root.
    #[cfg(feature = "test-support")]
    pub fn before_manifest_revalidation_for_test(
        root: &std::path::Path,
        callback: impl FnOnce() + Send + 'static,
    ) -> io::Result<()> {
        if std::fs::canonicalize(root)? != root {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "noncanonical test root",
            ));
        }
        let mut hooks = BEFORE_MANIFEST_REVALIDATION
            .get_or_init(|| std::sync::Mutex::new(HashMap::new()))
            .lock()
            .expect("content test hook lock");
        if hooks.contains_key(root) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "content test hook already armed",
            ));
        }
        hooks.insert(root.to_path_buf(), Box::new(callback));
        Ok(())
    }

    pub fn bind(directory: ManagedTreeDirectory, authority: ManagedTransferAuthority) -> Self {
        Self {
            directory,
            authority,
            path_policy: ManagedContentPathPolicy::Managed,
        }
    }

    pub fn for_pack(mut self) -> Self {
        self.path_policy = ManagedContentPathPolicy::Pack;
        self
    }

    pub fn observe_manifest(
        self,
    ) -> Result<ManagedContentPlanningSession, ManagedContentManifestObservationFailure> {
        observe_transaction_manifest(self)
    }

    pub fn restore_staging_checkpoint(
        self,
        checkpoint: ManagedContentStagingCheckpoint,
        binding: [u8; 32],
    ) -> Result<ManagedContentRecovery, ManagedContentCheckpointError> {
        checkpoint.validate()?;
        if checkpoint.record.binding != binding
            || checkpoint.record.pack != (self.path_policy == ManagedContentPathPolicy::Pack)
        {
            return Err(ManagedContentCheckpointError::Invalid);
        }
        let root = self.directory.directory;
        if directory_incarnation(&root)? != checkpoint.record.root {
            return Err(ManagedContentCheckpointError::Changed);
        }
        inspect_staging_checkpoint(&root, &checkpoint, false)?;
        Ok(ManagedContentRecovery {
            state: Some(RecoveryState::StagingRollback {
                root,
                authority: self.authority,
                checkpoint,
            }),
        })
    }
}

fn observe_transaction_manifest(
    transaction_root: ManagedContentTransactionRoot,
) -> Result<ManagedContentPlanningSession, ManagedContentManifestObservationFailure> {
    let refuse = |error, root| ManagedContentManifestObservationFailure { error, root };
    let ManagedContentTransactionRoot {
        directory: ManagedTreeDirectory { directory: root },
        authority,
        path_policy,
    } = transaction_root;
    let manifest = match observe_file(&root, MANIFEST_NAME, MAX_MANIFEST_BYTES as u64, true, None) {
        Ok(observation) => observation,
        Err(error) => {
            return Err(refuse(
                public_observation_error(error, true),
                ManagedContentTransactionRoot {
                    directory: ManagedTreeDirectory { directory: root },
                    authority,
                    path_policy,
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
        remaining_bytes: path_policy.transaction_byte_limit(),
        path_policy,
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
        .is_none_or(|total| total > session.path_policy.planning_path_limit())
    {
        return Err(refuse(
            ManagedContentObservationError::TooManyPaths,
            session,
        ));
    }
    let mut batch_keys = BTreeSet::new();
    for path in &paths {
        if validate_content_path(session.path_policy, path).is_err() {
            return Err(refuse(ManagedContentObservationError::InvalidPath, session));
        }
        let key = path.key();
        if session.observed_paths.contains_key(&key) || !batch_keys.insert(key) {
            return Err(refuse(
                ManagedContentObservationError::DuplicatePath,
                session,
            ));
        }
    }
    if !transaction_parent_spellings_are_exact(
        session
            .observations
            .iter()
            .map(|observation| &observation.public.path)
            .chain(paths.iter()),
    ) {
        return Err(refuse(
            ManagedContentObservationError::NonPortableEntry,
            session,
        ));
    }

    let initial_observation_count = session.observations.len();
    let initial_remaining_bytes = session.remaining_bytes;
    let mut parents = HashMap::<String, TransactionParent>::new();
    for path in paths {
        let key = path.key();
        let exact_path = path.clone();
        let (parent_path, name) = split_content_path(&path);
        let parent_key = parent_path
            .as_ref()
            .map_or_else(String::new, |parent| parent.as_str().to_string());
        if !parents.contains_key(&parent_key) {
            let parent = match resolve_transaction_parent(&session.root, parent_path.as_ref()) {
                Ok(parent) => parent,
                Err(error) => {
                    return Err(refuse(public_observation_error(error, false), session));
                }
            };
            parents.insert(parent_key.clone(), parent);
        }
        let parent = parents
            .get(&parent_key)
            .expect("resolved transaction parent")
            .clone();
        if session.path_policy == ManagedContentPathPolicy::Managed && parent.directory().is_none()
        {
            return Err(refuse(
                ManagedContentObservationError::ParentUnavailable,
                session,
            ));
        }
        let observed = match parent.directory() {
            Some(parent) => match observe_file(
                parent,
                name.as_str(),
                MAX_CONTENT_FILE_BYTES,
                false,
                Some(&mut session.remaining_bytes),
            ) {
                Ok(observed) => observed,
                Err(error) => {
                    return Err(refuse(public_observation_error(error, false), session));
                }
            },
            None => ExactObservation {
                state: ManagedContentObservedState::Absent,
                guard: None,
                bytes: None,
            },
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
        debug_assert!(
            previous.is_none(),
            "prevalidated content path remains unique"
        );
    }
    let bindings = session
        .observations
        .iter()
        .filter_map(|observation| {
            observation
                .parent
                .directory()
                .map(|parent| (parent, &observation.name))
        })
        .collect::<Vec<_>>();
    if validate_path_name_bindings(session.path_policy, bindings.into_iter()).is_err() {
        for observation in session.observations.drain(initial_observation_count..) {
            session
                .observed_paths
                .remove(&observation.public.path.key());
        }
        session.remaining_bytes = initial_remaining_bytes;
        return Err(refuse(
            ManagedContentObservationError::NonPortableEntry,
            session,
        ));
    }
    Ok(session)
}

fn transaction_parent_spellings_are_exact<'a>(
    paths: impl Iterator<Item = &'a PortableRelativePath>,
) -> bool {
    let mut parents = BTreeMap::<PortablePathKey, PortableRelativePath>::new();
    paths
        .filter_map(|path| split_content_path(path).0)
        .all(|parent| {
            let key = parent.key();
            parents
                .insert(key, parent.clone())
                .is_none_or(|previous| previous == parent)
        })
}

fn finish_transaction_observation(
    session: ManagedContentPlanningSession,
    paths: Vec<PortableRelativePath>,
) -> Result<ManagedContentTransactionSession, ManagedContentPlanningObservationFailure> {
    if paths.len() > session.path_policy.final_path_limit() {
        return Err(ManagedContentPlanningObservationFailure {
            error: ManagedContentObservationError::TooManyPaths,
            session,
        });
    }
    let mut selected_keys = BTreeSet::new();
    for path in &paths {
        if validate_content_path(session.path_policy, path).is_err() {
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
    if validate_path_name_bindings(
        session.path_policy,
        session.observations.iter().filter_map(|observation| {
            observation
                .parent
                .directory()
                .map(|parent| (parent, &observation.name))
        }),
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
        path_policy,
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
        path_policy,
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

fn admit_observed_bytes(remaining: &mut u64, size: u64) -> Result<(), FileObservationFailure> {
    *remaining = remaining
        .checked_sub(size)
        .ok_or(FileObservationFailure::TransactionBudgetExceeded)?;
    Ok(())
}

fn split_content_path(
    path: &PortableRelativePath,
) -> (Option<PortableRelativePath>, PortableFileName) {
    let (parent, name) = match path.as_str().rsplit_once('/') {
        Some((parent, name)) => (
            Some(
                PortableRelativePath::new_exact(parent)
                    .expect("validated content parent remains portable"),
            ),
            name,
        ),
        None => (None, path.as_str()),
    };
    (
        parent,
        PortableFileName::new_exact(name).expect("validated content leaf remains portable"),
    )
}

fn resolve_transaction_parent(
    root: &ManagedDir,
    path: Option<&PortableRelativePath>,
) -> Result<TransactionParent, FileObservationFailure> {
    let Some(path) = path else {
        return Ok(TransactionParent::Resolved {
            directory: root.clone(),
        });
    };
    let mut parent = root.clone();
    for (index, segment) in path.as_str().split('/').enumerate() {
        parent = match parent.open_child_if_exists(segment) {
            Ok(Some(child)) => child,
            Ok(None) => {
                return Ok(TransactionParent::Missing {
                    path: path.clone(),
                    first_missing: index,
                });
            }
            Err(LoaderError::Verify(_)) => return Err(FileObservationFailure::NonPortableEntry),
            Err(_) => return Err(FileObservationFailure::Unavailable),
        };
    }
    Ok(TransactionParent::Resolved { directory: parent })
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
        let (_, watched) = groups.entry(parent.inner.identity).or_insert_with(|| {
            (
                parent,
                BTreeMap::<PortablePathKey, ManagedLogicalNameSpec>::new(),
            )
        });
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

fn validate_path_name_bindings<'a>(
    policy: ManagedContentPathPolicy,
    bindings: impl Iterator<Item = (&'a ManagedDir, &'a PortableFileName)>,
) -> Result<(), FileObservationFailure> {
    if policy == ManagedContentPathPolicy::Managed {
        return validate_managed_logical_name_bindings(bindings);
    }
    let mut groups =
        HashMap::<_, (&ManagedDir, BTreeMap<PortablePathKey, &PortableFileName>)>::new();
    for (parent, name) in bindings {
        let (_, watched) = groups
            .entry(parent.inner.identity)
            .or_insert_with(|| (parent, BTreeMap::new()));
        match watched.insert(name.key(), name) {
            Some(previous) if previous != name => {
                return Err(FileObservationFailure::NonPortableEntry);
            }
            Some(_) | None => {}
        }
    }
    let entry_limit = policy.final_path_limit();
    for (_, (parent, watched)) in groups {
        for entry in parent
            .entries_bounded(entry_limit)
            .map_err(|_| FileObservationFailure::Unavailable)?
        {
            let raw = entry
                .to_str()
                .ok_or(FileObservationFailure::NonPortableEntry)?;
            let name = PortableFileName::new_exact(raw)
                .map_err(|_| FileObservationFailure::NonPortableEntry)?;
            if let Some(expected) = watched.get(&name.key())
                && name != **expected
            {
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
    source: ManagedContentTransferSource,
}

#[derive(Clone, Copy)]
enum ManagedContentTransferSource {
    Remote,
    Observation(usize),
    External,
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

/// Complete exact verified set, already published inside the private transaction.
#[must_use = "complete content transfers must bind, commit, or cancel"]
pub struct ManagedContentCompleteTransfers {
    state: TransactionState,
}

#[must_use = "manifest binding must retain the complete verified transfer set"]
pub enum ManagedContentManifestBindOutcome {
    Bound(ManagedContentCompleteTransfers),
    Refused {
        error: ManagedContentPlanError,
        transfers: ManagedContentCompleteTransfers,
    },
}

impl fmt::Debug for ManagedContentManifestBindOutcome {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let variant = match self {
            Self::Bound(_) => "Bound",
            Self::Refused { .. } => "Refused",
        };
        formatter
            .debug_struct("ManagedContentManifestBindOutcome")
            .field("variant", &variant)
            .finish()
    }
}

impl fmt::Debug for ManagedContentCompleteTransfers {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentCompleteTransfers")
            .field("payloads", &self.state.payloads.len())
            .finish_non_exhaustive()
    }
}

impl ManagedContentTransferBatch {
    pub fn payload_count(&self) -> usize {
        self.payload_count
    }

    /// Emits only initial or geometrically advancing quiescent staged prefixes.
    pub fn checkpoint(
        &mut self,
        binding: [u8; 32],
    ) -> Result<Option<&ManagedContentStagingCheckpoint>, ManagedContentCheckpointError> {
        if self
            .state
            .checkpoint
            .as_ref()
            .is_some_and(|cache| cache.binding != binding)
        {
            return Err(ManagedContentCheckpointError::Invalid);
        }
        if !self.verified.is_empty() {
            return Ok(None);
        }
        staging_checkpoint(&mut self.state, binding, false)
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
            None => {
                debug_assert!(self.verified.is_empty());
                debug_assert_eq!(self.state.payloads.len(), self.state.planned_payloads.len());
                ManagedContentTransferStep::Complete(ManagedContentCompleteTransfers {
                    state: self.state,
                })
            }
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

    pub fn is_local(&self) -> bool {
        matches!(
            self.slot.source,
            ManagedContentTransferSource::Observation(_)
        )
    }

    pub fn is_external(&self) -> bool {
        matches!(self.slot.source, ManagedContentTransferSource::External)
    }

    pub fn start(
        self,
        client: TransferClient,
        url: reqwest::Url,
        retry: RetryPolicy,
        cancellation: TransferCancellation,
    ) -> Result<ManagedContentTransferTask, Self> {
        if !matches!(self.slot.source, ManagedContentTransferSource::Remote) {
            return Err(self);
        }
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
            source,
        } = slot;
        debug_assert!(matches!(source, ManagedContentTransferSource::Remote));
        Ok(ManagedContentTransferTask {
            task: start_create_only_transfer(client, url, target, contract, retry, cancellation),
            continuation: ManagedContentTransferContinuation {
                state,
                verified,
                cancellation: slot_cancellation,
                remaining,
                payload_count,
            },
        })
    }

    pub fn copy_local(
        self,
        cancellation: TransferCancellation,
    ) -> ManagedContentTransferSettlement {
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
            source,
        } = slot;
        let reader = match source {
            ManagedContentTransferSource::Observation(index) => state.mutations.get(index),
            ManagedContentTransferSource::Remote | ManagedContentTransferSource::External => None,
        }
        .and_then(|mutation| mutation.old_guard.as_ref())
        .ok_or(io::ErrorKind::NotFound)
        .and_then(|guard| {
            guard
                .bounded_reader(transfer_contract_limit(&contract))
                .map_err(|error| match error {
                    LoaderError::Io(error) => error.kind(),
                    _ => io::ErrorKind::Other,
                })
        });
        let outcome = match reader {
            Ok(reader) => {
                copy_create_only_transfer(target, Box::new(reader), contract, cancellation)
            }
            Err(kind) => fail_create_only_transfer(
                target,
                crate::download::TransferFailureKind::SourceRead(kind),
            ),
        };
        ManagedContentTransferSettlement {
            continuation: ManagedContentTransferContinuation {
                state,
                verified,
                cancellation: slot_cancellation,
                remaining,
                payload_count,
            },
            outcome,
        }
    }

    pub fn copy_external<R>(
        self,
        reader: R,
        cancellation: TransferCancellation,
    ) -> Result<ManagedContentTransferSettlement, Self>
    where
        R: std::io::Read,
    {
        if !self.is_external() {
            return Err(self);
        }
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
            source,
        } = slot;
        debug_assert!(matches!(source, ManagedContentTransferSource::External));
        let outcome = copy_create_only_transfer(
            target,
            Box::new(ExternalTransferReader(reader)),
            contract,
            cancellation,
        );
        Ok(ManagedContentTransferSettlement {
            continuation: ManagedContentTransferContinuation {
                state,
                verified,
                cancellation: slot_cancellation,
                remaining,
                payload_count,
            },
            outcome,
        })
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

struct ExternalTransferReader<R>(R);

impl<R: std::io::Read> std::io::Read for ExternalTransferReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        self.0.read(buffer)
    }
}

impl<R: std::io::Read> crate::download::LocalTransferReader for ExternalTransferReader<R> {
    fn finish(self: Box<Self>) -> io::Result<()> {
        Ok(())
    }

    fn cancel(self: Box<Self>) {}
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
    pub fn reports(
        &self,
    ) -> impl ExactSizeIterator<Item = (&ManagedContentPayloadId, &TransferReport)> {
        self.state
            .planned_payloads
            .iter()
            .zip(&self.state.payloads)
            .map(|(planned, payload)| (&planned.id, &payload.report))
    }

    pub fn bind_manifest(mut self, body: Vec<u8>) -> ManagedContentManifestBindOutcome {
        let error = if !self.state.manifest_body.is_empty() || body.is_empty() {
            Some(ManagedContentPlanError::InvalidManifest)
        } else if body.len() > MAX_MANIFEST_BYTES {
            Some(ManagedContentPlanError::ManifestTooLarge)
        } else if body.len() as u64 > self.state.remaining_manifest_bytes {
            Some(ManagedContentPlanError::TransactionBudgetExceeded)
        } else {
            None
        };
        if let Some(error) = error {
            return ManagedContentManifestBindOutcome::Refused {
                error,
                transfers: self,
            };
        }
        self.state.manifest_body = body.into_boxed_slice();
        ManagedContentManifestBindOutcome::Bound(self)
    }

    pub fn stage(self) -> ManagedContentStageOutcome {
        if self.state.manifest_body.is_empty() {
            return ManagedContentStageOutcome::Unwind(drive_rollback(self.state, true));
        }
        ManagedContentStageOutcome::Ready(ManagedContentReadyTransaction { state: self.state })
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        drive_rollback(self.state, true)
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
    parent: TransactionParent,
    name: PortableFileName,
    observed: ManagedContentObservedState,
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
    source: ManagedContentTransferSource,
}

struct StagedPayload {
    name: PortableFileName,
    report: TransferReport,
    guard: Option<ManagedFileGuard>,
}

struct CreatedTransactionParent {
    parent: ManagedDir,
    name: PortableFileName,
    cleanup: CleanupDirectoryState,
}

struct TransactionState {
    root: ManagedDir,
    authority: ManagedTransferAuthority,
    path_policy: ManagedContentPathPolicy,
    private_name: PortableFileName,
    private: ManagedDir,
    stage: ManagedDir,
    backup: ManagedDir,
    manifest: ExactObservation,
    manifest_body: Box<[u8]>,
    remaining_manifest_bytes: u64,
    mutations: Vec<TransactionMutation>,
    read_preconditions: Vec<PathObservationAuthority>,
    planned_payloads: Vec<PlannedPayload>,
    staged_by_id: BTreeMap<ManagedContentPayloadId, usize>,
    payloads: Vec<StagedPayload>,
    checkpoint: Option<StagingCheckpointCache>,
    manifest_claimed: bool,
    manifest_installed: Option<ManagedFileGuard>,
    manifest_publication_started: bool,
    manifest_committed: bool,
    terminal_failure: ManagedContentTransactionFailure,
    stage_cleanup: CleanupDirectoryState,
    backup_cleanup: CleanupDirectoryState,
    private_cleanup: CleanupDirectoryState,
    created_parents: Vec<CreatedTransactionParent>,
    #[cfg(any(test, feature = "test-support"))]
    before_manifest_revalidation: Option<BeforeManifestRevalidation>,
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
    if private_entries
        .iter()
        .filter(|entry| {
            entry
                .to_str()
                .is_some_and(|name| name.starts_with(".axial-content-"))
        })
        .count()
        >= MAX_CONTENT_PRIVATE_DIRECTORIES
    {
        return ManagedContentPreparationOutcome::Refused {
            error: ManagedContentPreparationError::PrivateNamespaceExhausted,
            session,
        };
    }
    let private_name =
        PortableFileName::new_exact(&format!(".axial-content-{}", uuid::Uuid::new_v4().simple()))
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

    let group_authority = ManagedTransferAuthority::retain(Arc::new(ManagedContentTransferGroup {
        _state_authority: session.authority,
    }));
    let mut planned_payloads = Vec::with_capacity(plan.payloads.len());
    for payload in &plan.payloads {
        let source = match &payload.source {
            ManagedContentPayloadSourcePlan::Remote => ManagedContentTransferSource::Remote,
            ManagedContentPayloadSourcePlan::Observation(source) => {
                ManagedContentTransferSource::Observation(
                    session
                        .observations
                        .iter()
                        .position(|observation| observation.public.path == *source)
                        .expect("validated local payload source remains observed"),
                )
            }
            ManagedContentPayloadSourcePlan::External => ManagedContentTransferSource::External,
        };
        planned_payloads.push(PlannedPayload {
            id: payload.id.clone(),
            contract: payload.contract.clone(),
            authority: group_authority.retained(),
            source,
        });
    }
    let slots = match admit_transfer_slots(&stage, &planned_payloads, 0) {
        Ok(slots) => slots,
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
                observed: observed.public.state,
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
    let remaining_manifest_bytes = plan.remaining_manifest_bytes;
    let (manifest_body, _) = plan.manifest.into_parts();
    let stage_cleanup = CleanupDirectoryState::Known(stage.clone());
    let backup_cleanup = CleanupDirectoryState::Known(backup.clone());
    let private_cleanup = CleanupDirectoryState::Known(private.clone());
    #[cfg(any(test, feature = "test-support"))]
    let before_manifest_revalidation = BEFORE_MANIFEST_REVALIDATION.get().and_then(|hooks| {
        let mut hooks = hooks.lock().expect("content test hook lock");
        let root = hooks
            .keys()
            .find(|path| session.root.validate_absolute_projection(path).is_ok())
            .cloned()?;
        hooks.remove(&root)
    });
    ManagedContentPreparationOutcome::Prepared(ManagedContentPreparedTransaction {
        state: TransactionState {
            root: session.root,
            authority: group_authority,
            path_policy: session.path_policy,
            private_name,
            private,
            stage,
            backup,
            manifest: session.manifest,
            manifest_body,
            remaining_manifest_bytes,
            mutations,
            read_preconditions: session.read_preconditions,
            planned_payloads,
            staged_by_id: BTreeMap::new(),
            payloads: Vec::new(),
            checkpoint: None,
            manifest_claimed: false,
            manifest_installed: None,
            manifest_publication_started: false,
            manifest_committed: false,
            terminal_failure: ManagedContentTransactionFailure::ObservationDrift,
            stage_cleanup,
            backup_cleanup,
            private_cleanup,
            created_parents: Vec::new(),
            #[cfg(any(test, feature = "test-support"))]
            before_manifest_revalidation,
        },
        slots,
    })
}

fn admit_transfer_slots(
    stage: &ManagedDir,
    planned: &[PlannedPayload],
    start: usize,
) -> io::Result<Vec<ManagedContentTransferSlot>> {
    let end = start
        .saturating_add(MAX_TRANSIENT_STAGE_MEMBERS)
        .min(planned.len());
    if start == end {
        return Ok(Vec::new());
    }
    let names = (start..end)
        .map(|index| {
            LeafName::new(format!("payload-{index}"))
                .expect("bounded payload index is a portable leaf")
        })
        .collect();
    let destinations = stage
        .inner
        .directory
        .admit_transient_destinations(names)?
        .into_destinations();
    Ok(planned[start..end]
        .iter()
        .zip(destinations)
        .map(|(payload, destination)| ManagedContentTransferSlot {
            id: payload.id.clone(),
            contract: payload.contract.clone(),
            target: CreateOnlyTransferTarget::new(destination, payload.authority.retained()),
            cancellation: ManagedContentSlotCancellation {
                id: payload.id.clone(),
                authority: payload.authority.retained(),
            },
            source: payload.source,
        })
        .collect())
}

fn plan_matches_session(
    session: &ManagedContentTransactionSession,
    plan: &ManagedContentMutationPlan,
) -> bool {
    if session.observations.len() != plan.mutations.len() {
        return false;
    }
    if !Arc::ptr_eq(&session.manifest_session, plan.manifest.session()) {
        return false;
    }
    if session.path_policy != plan.manifest.path_policy() {
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
        let payload_count = state.planned_payloads.len();
        ManagedContentTransferBatch {
            state,
            verified: Vec::with_capacity(slots.len()),
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
    let planned_index = state.payloads.len() + verified.len();
    let planned = &state.planned_payloads[planned_index];
    let authority_matches = transfer_outcome_shares_authority(&outcome, &planned.authority);
    let contract_matches = match &outcome {
        TransferOutcome::Complete(value) => {
            report_matches_contract(value.report(), &planned.contract)
        }
        _ => true,
    };
    if authority_matches
        && contract_matches
        && let TransferOutcome::Complete(value) = outcome
    {
        verified.push(ManagedContentVerifiedTransfer {
            cancellation,
            verified: value,
        });
        if remaining.is_empty() {
            let state = match publish_verified_chunk(state, verified) {
                StageChunkOutcome::Published(state) => state,
                StageChunkOutcome::Unwind(outcome) => {
                    return ManagedContentTransferAdvance::Unwind(outcome);
                }
            };
            let next = match admit_transfer_slots(
                &state.stage,
                &state.planned_payloads,
                state.payloads.len(),
            ) {
                Ok(next) => next,
                Err(_) => {
                    return ManagedContentTransferAdvance::Unwind(drive_rollback(state, false));
                }
            };
            return ManagedContentTransferAdvance::Continue(ManagedContentTransferBatch {
                state,
                verified: Vec::with_capacity(next.len()),
                remaining: next.into(),
                payload_count,
            });
        }
        return ManagedContentTransferAdvance::Continue(ManagedContentTransferBatch {
            state,
            verified,
            remaining,
            payload_count,
        });
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

enum StageChunkOutcome {
    Published(TransactionState),
    Unwind(ManagedContentTransactionOutcome),
}

fn publish_verified_chunk(
    state: TransactionState,
    verified: Vec<ManagedContentVerifiedTransfer>,
) -> StageChunkOutcome {
    let offset = state.payloads.len();
    debug_assert!(!verified.is_empty());
    debug_assert!(verified.len() <= MAX_TRANSIENT_STAGE_MEMBERS);
    debug_assert!(offset + verified.len() <= state.planned_payloads.len());
    let mut stages = Vec::with_capacity(verified.len());
    let mut retained = Vec::with_capacity(verified.len());
    let mut cancellations = Vec::with_capacity(verified.len());
    for (planned, transfer) in state.planned_payloads[offset..].iter().zip(verified) {
        let (stage, report, authority) = transfer.verified.into_content_stage();
        stages.push(stage);
        retained.push((planned.id.clone(), report, authority));
        cancellations.push(transfer.cancellation);
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
            return StageChunkOutcome::Unwind(cancel_transfer_batch(
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
        TransferOutcome::Unsettled(obligation) => obligation.shares_retained_authority(authority),
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
                source: _,
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
    retained: Vec<(
        ManagedContentPayloadId,
        TransferReport,
        ManagedTransferAuthority,
    )>,
    cancellations: Vec<ManagedContentSlotCancellation>,
    outcome: TransientPublicationBatchOutcome,
) -> StageChunkOutcome {
    match outcome {
        TransientPublicationBatchOutcome::Published(files) => {
            drop(cancellations);
            let offset = state.payloads.len();
            let mut members = files.into_iter().zip(retained).enumerate();
            while let Some((local_index, (file, (id, report, authority)))) = members.next() {
                let index = offset + local_index;
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
                            |(local_index, (file, (id, report, authority)))| {
                                StageRecoveryMember::Published {
                                    index: offset + local_index,
                                    id,
                                    report,
                                    authority,
                                    file,
                                }
                            },
                        ));
                        return StageChunkOutcome::Unwind(
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
                state.staged_by_id.insert(id.clone(), state.payloads.len());
                state.payloads.push(StagedPayload {
                    name,
                    report,
                    guard: Some(guard),
                });
                drop(authority);
            }
            StageChunkOutcome::Published(state)
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
            StageChunkOutcome::Unwind(cancel_transfer_batch(state, verified, VecDeque::new()))
        }
        TransientPublicationBatchOutcome::Partial { members, .. } => {
            drop(cancellations);
            StageChunkOutcome::Unwind(ManagedContentTransactionOutcome::RecoveryRequired(
                ManagedContentRecovery {
                    state: Some(RecoveryState::StagePartial {
                        transaction: state,
                        retained,
                        members,
                    }),
                },
            ))
        }
        TransientPublicationBatchOutcome::Pending(obligation) => {
            drop(cancellations);
            StageChunkOutcome::Unwind(ManagedContentTransactionOutcome::RecoveryRequired(
                ManagedContentRecovery {
                    state: Some(RecoveryState::StagePending {
                        transaction: state,
                        retained,
                        obligation: Some(obligation),
                    }),
                },
            ))
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
    pub fn prepare_publication(mut self, binding: [u8; 32]) -> ManagedContentStageOutcome {
        match self.checkpoint(binding) {
            Ok(_)
            | Err(
                ManagedContentCheckpointError::Unsupported
                | ManagedContentCheckpointError::Capacity,
            ) => {}
            Err(_) => return ManagedContentStageOutcome::Unwind(drive_rollback(self.state, false)),
        }
        if prepare_publication_prefix(&mut self.state).is_err() {
            return ManagedContentStageOutcome::Unwind(drive_rollback(self.state, false));
        }
        ManagedContentStageOutcome::Ready(self)
    }

    pub fn checkpoint(
        &mut self,
        binding: [u8; 32],
    ) -> Result<&ManagedContentStagingCheckpoint, ManagedContentCheckpointError> {
        staging_checkpoint(&mut self.state, binding, true)?
            .ok_or(ManagedContentCheckpointError::Invalid)
    }

    pub fn commit(self) -> ManagedContentTransactionOutcome {
        drive_commit(self.state, |_| true)
    }

    /// Offers optional create-only recovery evidence before publishing the manifest.
    /// Returning false stops publication and retains ownership through rollback.
    pub fn commit_with_checkpoint(
        self,
        binding: [u8; 32],
        persist: impl FnOnce(&ManagedContentStagingCheckpoint) -> bool,
    ) -> ManagedContentTransactionOutcome {
        let mut ready = match self.prepare_publication(binding) {
            ManagedContentStageOutcome::Ready(ready) => ready,
            ManagedContentStageOutcome::Unwind(outcome) => return outcome,
        };
        match ready.checkpoint(binding) {
            Ok(_)
            | Err(
                ManagedContentCheckpointError::Unsupported
                | ManagedContentCheckpointError::Capacity,
            ) => {}
            Err(_) => return drive_rollback(ready.state, false),
        }
        drive_commit(ready.state, |state| {
            match published_checkpoint(state, binding) {
                Ok(Some(checkpoint)) => persist(&checkpoint),
                Ok(None)
                | Err(
                    ManagedContentCheckpointError::Unsupported
                    | ManagedContentCheckpointError::Capacity,
                ) => true,
                Err(_) => false,
            }
        })
    }

    pub fn cancel(self) -> ManagedContentTransactionOutcome {
        drive_rollback(self.state, true)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedContentCheckpointError {
    Unsupported,
    Capacity,
    Changed,
    Invalid,
}

/// Comparison evidence only. Restoring always requires a freshly admitted root.
#[derive(Clone)]
pub struct ManagedContentStagingCheckpoint {
    record: StagingCheckpoint,
}

struct StagingCheckpointCache {
    binding: [u8; 32],
    checkpoint: Result<ManagedContentStagingCheckpoint, ManagedContentCheckpointError>,
    remaining: usize,
}

impl fmt::Debug for ManagedContentStagingCheckpoint {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ManagedContentStagingCheckpoint")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingCheckpoint {
    schema: u32,
    binding: [u8; 32],
    pack: bool,
    root: [u8; 32],
    private_name: String,
    private: [u8; 32],
    stage: [u8; 32],
    backup: [u8; 32],
    mutation_count: usize,
    #[serde(deserialize_with = "staging_directories")]
    directories: BTreeMap<String, Option<[u8; 32]>>,
    #[serde(
        default,
        skip_serializing_if = "BTreeMap::is_empty",
        deserialize_with = "staging_directories"
    )]
    created_parents: BTreeMap<String, [u8; 32]>,
    files: Vec<StagingFile>,
    payloads: Vec<StagingPayload>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    published: Vec<PublishedPayload>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingFile {
    path: String,
    proof: Option<StagingFileProof>,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingPayload {
    name: String,
    proof: StagingFileProof,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct PublishedPayload {
    path: String,
    payload_index: usize,
    proof: StagingFileProof,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StagingFileProof {
    revision: [u8; 32],
    size: u64,
    sha512: String,
}

fn staging_directories<'de, D: serde::Deserializer<'de>, V: Deserialize<'de>>(
    deserializer: D,
) -> Result<BTreeMap<String, V>, D::Error> {
    struct Directories<V>(std::marker::PhantomData<V>);
    impl<'de, V: Deserialize<'de>> serde::de::Visitor<'de> for Directories<V> {
        type Value = BTreeMap<String, V>;
        fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            formatter.write_str("bounded distinct content parent witnesses")
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut directories = BTreeMap::new();
            while let Some((path, witness)) = map.next_entry()? {
                if directories.insert(path, witness).is_some()
                    || directories.len() > MAX_STAGING_CHECKPOINT_BYTES / 4
                {
                    return Err(serde::de::Error::custom("invalid content parent witnesses"));
                }
            }
            Ok(directories)
        }
    }
    deserializer.deserialize_map(Directories(std::marker::PhantomData))
}

impl ManagedContentStagingCheckpoint {
    pub fn encode(&self, max_bytes: usize) -> Result<String, ManagedContentCheckpointError> {
        if max_bytes == 0 || max_bytes > MAX_STAGING_CHECKPOINT_BYTES {
            return Err(ManagedContentCheckpointError::Capacity);
        }
        self.validate()?;
        struct BoundedJson {
            bytes: Vec<u8>,
            limit: usize,
        }
        impl io::Write for BoundedJson {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                    return Err(io::Error::other("content checkpoint exceeds its bound"));
                }
                self.bytes.extend_from_slice(bytes);
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut output = BoundedJson {
            bytes: Vec::new(),
            limit: max_bytes,
        };
        serde_json::to_writer(&mut output, &self.record)
            .map_err(|_| ManagedContentCheckpointError::Capacity)?;
        String::from_utf8(output.bytes).map_err(|_| ManagedContentCheckpointError::Invalid)
    }

    pub fn decode(encoded: &str) -> Result<Self, ManagedContentCheckpointError> {
        if encoded.len() > MAX_STAGING_CHECKPOINT_BYTES {
            return Err(ManagedContentCheckpointError::Capacity);
        }
        let checkpoint = Self {
            record: serde_json::from_str(encoded)
                .map_err(|_| ManagedContentCheckpointError::Invalid)?,
        };
        checkpoint.validate()?;
        Ok(checkpoint)
    }

    fn validate(&self) -> Result<(), ManagedContentCheckpointError> {
        let invalid = ManagedContentCheckpointError::Invalid;
        let record = &self.record;
        let policy = if record.pack {
            ManagedContentPathPolicy::Pack
        } else {
            ManagedContentPathPolicy::Managed
        };
        let suffix = record
            .private_name
            .strip_prefix(".axial-content-")
            .ok_or(invalid)?;
        if record.schema != 1
            || suffix.len() != 32
            || !suffix
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            || record.mutation_count > policy.final_path_limit()
            || record.files.is_empty()
            || record.files.len() > policy.planning_path_limit() + 1
            || record.payloads.len() > record.mutation_count
            || record
                .directories
                .len()
                .saturating_add(record.created_parents.len())
                > MAX_STAGING_CHECKPOINT_BYTES / 4
        {
            return Err(invalid);
        }
        let mut paths = BTreeSet::new();
        let mut required_directories = BTreeSet::new();
        let mut creatable_parents = BTreeSet::new();
        let mut bytes = 0u64;
        for file in &record.files {
            let path = PortableRelativePath::new_exact(&file.path).map_err(|_| invalid)?;
            if !paths.insert(path.key())
                || (file.path != MANIFEST_NAME && validate_content_path(policy, &path).is_err())
            {
                return Err(invalid);
            }
            if let Some(proof) = &file.proof {
                validate_ready_file(
                    proof,
                    if file.path == MANIFEST_NAME {
                        MAX_MANIFEST_BYTES as u64
                    } else {
                        MAX_CONTENT_FILE_BYTES
                    },
                )?;
                bytes = bytes.checked_add(proof.size).ok_or(invalid)?;
            }
            let mut prefix = String::new();
            let mut missing = false;
            let segments = file.path.split('/').collect::<Vec<_>>();
            for segment in &segments[..segments.len() - 1] {
                if !prefix.is_empty() {
                    prefix.push('/');
                }
                prefix.push_str(segment);
                if missing {
                    if record.created_parents.contains_key(&prefix) {
                        creatable_parents.insert(prefix.clone());
                    }
                    continue;
                }
                required_directories.insert(prefix.clone());
                match record.directories.get(&prefix).ok_or(invalid)? {
                    Some(_) => {}
                    None if file.proof.is_none() => {
                        if record.created_parents.is_empty() {
                            break;
                        }
                        missing = true;
                        if record.created_parents.contains_key(&prefix) {
                            creatable_parents.insert(prefix.clone());
                        }
                    }
                    None => return Err(invalid),
                }
            }
        }
        if !record.files.iter().any(|file| file.path == MANIFEST_NAME)
            || required_directories.len() != record.directories.len()
            || record.mutation_count >= record.files.len()
        {
            return Err(invalid);
        }
        let mut parent_keys = BTreeMap::new();
        for path in record.directories.keys() {
            let portable = PortableRelativePath::new_exact(path).map_err(|_| invalid)?;
            if parent_keys.insert(portable.key(), path).is_some() {
                return Err(invalid);
            }
        }
        for path in record.created_parents.keys() {
            let portable = PortableRelativePath::new_exact(path).map_err(|_| invalid)?;
            let (parent, _) = path.rsplit_once('/').unwrap_or(("", path));
            if parent_keys
                .insert(portable.key(), path)
                .is_some_and(|prior| prior != path)
                || !creatable_parents.contains(path)
                || (!parent.is_empty()
                    && !record.directories.get(parent).is_some_and(Option::is_some)
                    && !record.created_parents.contains_key(parent))
            {
                return Err(invalid);
            }
        }
        for (index, payload) in record.payloads.iter().enumerate() {
            if payload.name != format!("payload-{index}") {
                return Err(invalid);
            }
            validate_ready_file(&payload.proof, MAX_CONTENT_FILE_BYTES)?;
            bytes = bytes.checked_add(payload.proof.size).ok_or(invalid)?;
        }
        if !record.published.is_empty() {
            if record.published.len() != record.mutation_count
                || record.payloads.len() != record.mutation_count
                || record.files[0].path != MANIFEST_NAME
            {
                return Err(invalid);
            }
            let mut payloads = BTreeSet::new();
            for (index, published) in record.published.iter().enumerate() {
                let before = &record.files[index + 1];
                let payload = record
                    .payloads
                    .get(published.payload_index)
                    .ok_or(invalid)?;
                if published.path != before.path
                    || before.proof.is_some()
                    || !payloads.insert(published.payload_index)
                    || published.proof.size != payload.proof.size
                    || published.proof.sha512 != payload.proof.sha512
                {
                    return Err(invalid);
                }
                validate_ready_file(&published.proof, MAX_CONTENT_FILE_BYTES)?;
            }
        }
        if bytes > policy.transaction_byte_limit() {
            return Err(invalid);
        }
        Ok(())
    }
}

fn validate_ready_file(
    proof: &StagingFileProof,
    limit: u64,
) -> Result<(), ManagedContentCheckpointError> {
    if proof.size > limit
        || proof.sha512.len() != 128
        || !proof
            .sha512
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(ManagedContentCheckpointError::Invalid);
    }
    Ok(())
}

fn checkpoint_io(error: io::Error) -> ManagedContentCheckpointError {
    if error.kind() == io::ErrorKind::Unsupported {
        ManagedContentCheckpointError::Unsupported
    } else {
        ManagedContentCheckpointError::Changed
    }
}

fn checkpoint_loader(error: LoaderError) -> ManagedContentCheckpointError {
    match error {
        LoaderError::Io(error) => checkpoint_io(error),
        _ => ManagedContentCheckpointError::Changed,
    }
}

fn directory_incarnation(
    directory: &ManagedDir,
) -> Result<[u8; 32], ManagedContentCheckpointError> {
    directory
        .revalidate()
        .map_err(|_| ManagedContentCheckpointError::Changed)?;
    let witness = directory
        .inner
        .directory
        .incarnation_witness()
        .map_err(checkpoint_io)?;
    directory
        .revalidate()
        .map_err(|_| ManagedContentCheckpointError::Changed)?;
    Ok(witness)
}

fn checkpoint_file(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    sha512: String,
) -> Result<StagingFileProof, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    if !directory
        .file_guard_matches(name, guard)
        .map_err(|_| changed)?
    {
        return Err(changed);
    }
    let revision = guard
        .identity
        .with_capability(|file| file.revision_witness().map_err(LoaderError::from))
        .map_err(checkpoint_loader)?;
    if !directory
        .file_guard_matches(name, guard)
        .map_err(|_| changed)?
    {
        return Err(changed);
    }
    Ok(StagingFileProof {
        revision,
        size: guard.size(),
        sha512,
    })
}

fn charge_checkpoint(
    remaining: &mut usize,
    value: &impl Serialize,
) -> Result<(), ManagedContentCheckpointError> {
    let size = serde_json::to_vec(value)
        .map_err(|_| ManagedContentCheckpointError::Invalid)?
        .len();
    *remaining = remaining
        .checked_sub(size + 1)
        .ok_or(ManagedContentCheckpointError::Capacity)?;
    Ok(())
}

fn staging_checkpoint(
    state: &mut TransactionState,
    binding: [u8; 32],
    final_ready: bool,
) -> Result<Option<&ManagedContentStagingCheckpoint>, ManagedContentCheckpointError> {
    if let Some(cache) = &state.checkpoint {
        if cache.binding != binding {
            return Err(ManagedContentCheckpointError::Invalid);
        }
        let checkpoint = cache.checkpoint.as_ref().map_err(|error| *error)?;
        if !final_ready
            && (state.payloads.len() == state.planned_payloads.len()
                || state.payloads.len()
                    < checkpoint
                        .record
                        .payloads
                        .len()
                        .saturating_mul(2)
                        .max(MAX_TRANSIENT_STAGE_MEMBERS))
        {
            return Ok(None);
        }
    }
    let initial = state.checkpoint.is_none();
    let mut cache = state.checkpoint.take().unwrap_or_else(|| {
        let (checkpoint, remaining) = match capture_staging_checkpoint(state, binding) {
            Ok((checkpoint, remaining)) => (Ok(checkpoint), remaining),
            Err(error) => (Err(error), 0),
        };
        StagingCheckpointCache {
            binding,
            checkpoint,
            remaining,
        }
    });
    let update = (|| {
        let checkpoint = cache.checkpoint.as_mut().map_err(|error| *error)?;
        append_staging_payloads(state, checkpoint, &mut cache.remaining)?;
        if final_ready {
            append_staging_parents(state, checkpoint, &mut cache.remaining)?;
        }
        if final_ready
            && (!unpublished_before_bindings_match(state)
                || state.payloads.len() != state.planned_payloads.len())
        {
            return Err(ManagedContentCheckpointError::Changed);
        }
        if initial || final_ready {
            checkpoint.validate()?;
            inspect_staging_checkpoint(&state.root, checkpoint, true)?;
        }
        Ok(())
    })();
    if let Err(error) = update {
        cache.checkpoint = Err(error);
    }
    state.checkpoint = Some(cache);
    state
        .checkpoint
        .as_ref()
        .expect("checkpoint capture retains its proof or refusal")
        .checkpoint
        .as_ref()
        .map(Some)
        .map_err(|error| *error)
}

fn unpublished_before_bindings_match(state: &TransactionState) -> bool {
    revalidate_all(state)
        && !state.manifest_claimed
        && !state.manifest_publication_started
        && !state.manifest_committed
        && state.manifest_installed.is_none()
        && state.mutations.iter().all(|mutation| {
            !mutation.claimed && !mutation.installed && mutation.installed_guard.is_none()
        })
}

fn capture_staging_checkpoint(
    state: &TransactionState,
    binding: [u8; 32],
) -> Result<(ManagedContentStagingCheckpoint, usize), ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    if !state.created_parents.is_empty() || !unpublished_before_bindings_match(state) {
        return Err(changed);
    }
    let mut remaining = MAX_STAGING_CHECKPOINT_BYTES - 1024;
    let mut files = Vec::new();
    let mut capture = |path: String,
                       parent: Option<&ManagedDir>,
                       guard: Option<&ManagedFileGuard>,
                       observed: &ManagedContentObservedState| {
        let proof = match (parent, guard, observed) {
            (Some(parent), Some(guard), ManagedContentObservedState::Exact { size, sha512 })
                if guard.size() == *size =>
            {
                Some(checkpoint_file(
                    parent,
                    path.rsplit('/').next().ok_or(changed)?,
                    guard,
                    sha512.to_string(),
                )?)
            }
            (_, None, ManagedContentObservedState::Absent) => None,
            _ => return Err(changed),
        };
        let file = StagingFile { path, proof };
        charge_checkpoint(&mut remaining, &file)?;
        files.push(file);
        Ok(())
    };
    capture(
        MANIFEST_NAME.into(),
        Some(&state.root),
        state.manifest.guard.as_ref(),
        &state.manifest.state,
    )?;
    for mutation in &state.mutations {
        let path = match &mutation.parent {
            TransactionParent::Missing { path, .. } => {
                format!("{}/{}", path.as_str(), mutation.name.as_str())
            }
            TransactionParent::Resolved { directory } => {
                let relative = directory
                    .inner
                    .path
                    .strip_prefix(&state.root.inner.path)
                    .map_err(|_| changed)?;
                if relative.as_os_str().is_empty() {
                    mutation.name.as_str().to_string()
                } else {
                    format!(
                        "{}/{}",
                        PortableRelativePath::from_path(relative)
                            .map_err(|_| changed)?
                            .as_str(),
                        mutation.name.as_str()
                    )
                }
            }
        };
        capture(
            path,
            mutation.parent.directory(),
            mutation.old_guard.as_ref(),
            &mutation.observed,
        )?;
    }
    for observation in &state.read_preconditions {
        capture(
            observation.public.path.as_str().to_string(),
            observation.parent.directory(),
            observation.guard.as_ref(),
            &observation.public.state,
        )?;
    }
    let mut directories = BTreeMap::<String, Option<[u8; 32]>>::new();
    let mut known = BTreeMap::from([(String::new(), state.root.clone())]);
    for file in &files {
        let mut prefix = String::new();
        let mut parent = state.root.clone();
        let segments = file.path.split('/').collect::<Vec<_>>();
        for segment in &segments[..segments.len() - 1] {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if let Some(witness) = directories.get(&prefix) {
                if witness.is_none() {
                    break;
                }
                parent = known.get(&prefix).ok_or(changed)?.clone();
                continue;
            }
            let child = parent.open_child_if_exists(segment).map_err(|_| changed)?;
            let witness = child.as_ref().map(directory_incarnation).transpose()?;
            charge_checkpoint(&mut remaining, &(&prefix, witness))?;
            directories.insert(prefix.clone(), witness);
            let Some(child) = child else {
                break;
            };
            known.insert(prefix.clone(), child.clone());
            parent = child;
        }
    }
    Ok((
        ManagedContentStagingCheckpoint {
            record: StagingCheckpoint {
                schema: 1,
                binding,
                pack: state.path_policy == ManagedContentPathPolicy::Pack,
                root: directory_incarnation(&state.root)?,
                private_name: state.private_name.as_str().to_string(),
                private: directory_incarnation(&state.private)?,
                stage: directory_incarnation(&state.stage)?,
                backup: directory_incarnation(&state.backup)?,
                mutation_count: state.mutations.len(),
                directories,
                created_parents: BTreeMap::new(),
                files,
                payloads: Vec::new(),
                published: Vec::new(),
            },
        },
        remaining,
    ))
}

fn append_staging_payloads(
    state: &TransactionState,
    checkpoint: &mut ManagedContentStagingCheckpoint,
    remaining: &mut usize,
) -> Result<(), ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let record = &mut checkpoint.record;
    if directory_incarnation(&state.root)? != record.root
        || directory_incarnation(&state.private)? != record.private
        || directory_incarnation(&state.stage)? != record.stage
        || directory_incarnation(&state.backup)? != record.backup
        || record.payloads.len() > state.payloads.len()
    {
        return Err(changed);
    }
    // Previously captured proofs stay unchanged even if an external writer drifts.
    // Final publication and cold recovery still verify the complete exact tree.
    for payload in &state.payloads[record.payloads.len()..] {
        let guard = payload.guard.as_ref().ok_or(changed)?;
        let sha512 = match payload.report.digests().sha512() {
            Some(hash) => hex_lower(hash),
            None => state
                .stage
                .sha512_guarded_file(payload.name.as_str(), guard, MAX_CONTENT_FILE_BYTES)
                .map_err(|_| changed)?,
        };
        let payload = StagingPayload {
            name: payload.name.as_str().to_string(),
            proof: checkpoint_file(&state.stage, payload.name.as_str(), guard, sha512)?,
        };
        charge_checkpoint(remaining, &payload)?;
        record.payloads.push(payload);
    }
    Ok(())
}

fn append_staging_parents(
    state: &TransactionState,
    checkpoint: &mut ManagedContentStagingCheckpoint,
    remaining: &mut usize,
) -> Result<(), ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    for created in &state.created_parents {
        let CleanupDirectoryState::Known(directory) = &created.cleanup else {
            return Err(changed);
        };
        let path = directory
            .inner
            .path
            .strip_prefix(&state.root.inner.path)
            .map_err(|_| changed)?;
        let path = PortableRelativePath::from_path(path)
            .map_err(|_| changed)?
            .as_str()
            .to_string();
        if !checkpoint.record.created_parents.contains_key(&path) {
            let witness = directory_incarnation(directory)?;
            charge_checkpoint(remaining, &(&path, witness))?;
            checkpoint.record.created_parents.insert(path, witness);
        }
    }
    if checkpoint.record.created_parents.len() != state.created_parents.len() {
        return Err(changed);
    }
    Ok(())
}

fn published_checkpoint(
    state: &TransactionState,
    binding: [u8; 32],
) -> Result<Option<ManagedContentStagingCheckpoint>, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let cache = state.checkpoint.as_ref().ok_or(changed)?;
    if cache.binding != binding {
        return Err(ManagedContentCheckpointError::Invalid);
    }
    if state.mutations.is_empty()
        || state.mutations.iter().any(|mutation| {
            !matches!(mutation.observed, ManagedContentObservedState::Absent)
                || !matches!(mutation.result, ManagedContentPathResult::Download(_))
        })
    {
        return Ok(None);
    }
    let mut checkpoint = cache.checkpoint.as_ref().map_err(|error| *error)?.clone();
    if state.manifest_claimed
        || state.manifest_publication_started
        || state.manifest_committed
        || state.manifest_installed.is_some()
        || state.payloads.len() != state.mutations.len()
        || !checkpoint.record.published.is_empty()
    {
        return Err(changed);
    }
    state.root.settle().map_err(checkpoint_loader)?;
    let mut remaining = cache.remaining;
    for (index, mutation) in state.mutations.iter().enumerate() {
        if mutation.claimed || mutation.old_guard.is_some() || !mutation.installed {
            return Err(changed);
        }
        let ManagedContentPathResult::Download(id) = &mutation.result else {
            return Err(changed);
        };
        let payload_index = *state.staged_by_id.get(id).ok_or(changed)?;
        let payload = checkpoint
            .record
            .payloads
            .get(payload_index)
            .ok_or(changed)?;
        let guard = mutation.installed_guard.as_ref().ok_or(changed)?;
        let published = PublishedPayload {
            path: checkpoint
                .record
                .files
                .get(index + 1)
                .ok_or(changed)?
                .path
                .clone(),
            payload_index,
            proof: checkpoint_file(
                mutation.parent.resolved(),
                mutation.name.as_str(),
                guard,
                payload.proof.sha512.clone(),
            )?,
        };
        charge_checkpoint(&mut remaining, &published)?;
        checkpoint.record.published.push(published);
    }
    checkpoint.validate()?;
    inspect_staging_checkpoint(&state.root, &checkpoint, true)?;
    Ok(Some(checkpoint))
}

fn staging_public_bindings(
    root: &ManagedDir,
    checkpoint: &ManagedContentStagingCheckpoint,
    complete: bool,
) -> Result<StagingCleanup, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    if directory_incarnation(root)? != checkpoint.record.root {
        return Err(changed);
    }
    let mut directories = BTreeMap::from([(String::new(), Some(root.clone()))]);
    for (path, expected) in &checkpoint.record.directories {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
        let parent = directories
            .get(parent)
            .and_then(Option::as_ref)
            .ok_or(changed)?;
        let current = parent.open_child_if_exists(name).map_err(|_| changed)?;
        let witness = current.as_ref().map(directory_incarnation).transpose()?;
        let expected = expected.or_else(|| checkpoint.record.created_parents.get(path).copied());
        if witness != expected
            && !(witness.is_none() && checkpoint.record.created_parents.contains_key(path))
        {
            return Err(changed);
        }
        directories.insert(path.clone(), current);
    }
    let mut children = BTreeMap::<&str, BTreeSet<&str>>::new();
    let published = checkpoint
        .record
        .published
        .iter()
        .map(|payload| (payload.path.as_str(), &payload.proof))
        .collect::<BTreeMap<_, _>>();
    for path in published.keys() {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        children.entry(parent).or_default().insert(name);
    }
    let mut created_parents = Vec::new();
    for (path, witness) in &checkpoint.record.created_parents {
        let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
        children.entry(parent).or_default().insert(name);
        let parent = directories.get(parent).ok_or(changed)?.as_ref();
        let current = parent
            .map(|parent| checkpoint_directory(parent, name, *witness))
            .transpose()?
            .flatten();
        if let (Some(parent), Some(directory)) = (parent, &current) {
            created_parents.push(CreatedTransactionParent {
                parent: parent.clone(),
                name: PortableFileName::new_exact(name).map_err(|_| changed)?,
                cleanup: CleanupDirectoryState::Known(directory.clone()),
            });
        }
        directories.insert(path.clone(), current);
    }
    for path in checkpoint.record.created_parents.keys() {
        let Some(directory) = directories.get(path).and_then(Option::as_ref) else {
            continue;
        };
        let expected = children.get(path.as_str());
        for name in directory
            .entries_bounded(expected.map_or(1, |names| names.len().max(1)))
            .map_err(|_| changed)?
        {
            let name = name.to_str().ok_or(changed)?;
            if !expected.is_some_and(|names| names.contains(name))
                || (!published.contains_key(format!("{path}/{name}").as_str())
                    && directories
                        .get(&format!("{path}/{name}"))
                        .is_none_or(Option::is_none))
            {
                return Err(changed);
            }
        }
    }
    let mut bindings = Vec::new();
    let mut public_files = Vec::new();
    for file in &checkpoint.record.files {
        let (parent, name) = file
            .path
            .rsplit_once('/')
            .unwrap_or(("", file.path.as_str()));
        let Some(parent) = directories.get(parent).and_then(Option::as_ref) else {
            if file.proof.is_some() || (complete && published.contains_key(file.path.as_str())) {
                return Err(changed);
            }
            continue;
        };
        match (&file.proof, published.get(file.path.as_str())) {
            (Some(proof), None) => {
                admit_checkpoint_file(parent, name, proof)?;
            }
            (None, Some(proof)) => {
                if parent
                    .has_portably_exact_child_name(name)
                    .map_err(|_| changed)?
                {
                    public_files.push((
                        parent.clone(),
                        name.to_string(),
                        admit_checkpoint_file(parent, name, proof)?,
                    ));
                } else if complete {
                    return Err(changed);
                }
            }
            (None, None)
                if !parent
                    .has_portably_exact_child_name(name)
                    .map_err(|_| changed)? => {}
            _ => return Err(changed),
        }
        if file.path != MANIFEST_NAME {
            bindings.push((
                parent,
                PortableFileName::new_exact(name).map_err(|_| changed)?,
            ));
        }
    }
    validate_path_name_bindings(
        if checkpoint.record.pack {
            ManagedContentPathPolicy::Pack
        } else {
            ManagedContentPathPolicy::Managed
        },
        bindings.iter().map(|(parent, name)| (*parent, name)),
    )
    .map_err(|_| changed)?;
    Ok(StagingCleanup {
        created_parents,
        published: public_files,
        private: None,
        stage: None,
        backup: None,
        files: Vec::new(),
    })
}

fn admit_checkpoint_file(
    parent: &ManagedDir,
    name: &str,
    proof: &StagingFileProof,
) -> Result<ManagedFileGuard, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let guard = parent
        .inspect_regular_file(name)
        .map_err(|_| changed)?
        .ok_or(changed)?;
    if guard.size() != proof.size
        || guard
            .identity
            .with_capability(|file| file.revision_witness().map_err(LoaderError::from))
            .map_err(checkpoint_loader)?
            != proof.revision
        || parent
            .sha512_guarded_file(name, &guard, MAX_CONTENT_FILE_BYTES)
            .map_err(|_| changed)?
            != proof.sha512
        || !parent
            .file_guard_matches(name, &guard)
            .map_err(|_| changed)?
    {
        return Err(changed);
    }
    Ok(guard)
}

fn checkpoint_directory(
    parent: &ManagedDir,
    name: &str,
    proof: [u8; 32],
) -> Result<Option<ManagedDir>, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let directory = parent.open_child_if_exists(name).map_err(|_| changed)?;
    if directory
        .as_ref()
        .map(directory_incarnation)
        .transpose()?
        .is_some_and(|witness| witness != proof)
    {
        return Err(changed);
    }
    Ok(directory)
}

struct StagingCleanup {
    created_parents: Vec<CreatedTransactionParent>,
    published: Vec<(ManagedDir, String, ManagedFileGuard)>,
    private: Option<ManagedDir>,
    stage: Option<ManagedDir>,
    backup: Option<ManagedDir>,
    files: Vec<(String, ManagedFileGuard)>,
}

fn inspect_staging_checkpoint(
    root: &ManagedDir,
    checkpoint: &ManagedContentStagingCheckpoint,
    complete: bool,
) -> Result<StagingCleanup, ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let mut cleanup = staging_public_bindings(root, checkpoint, complete)?;
    if complete && cleanup.created_parents.len() != checkpoint.record.created_parents.len() {
        return Err(changed);
    }
    let record = &checkpoint.record;
    let Some(private) = checkpoint_directory(root, &record.private_name, record.private)? else {
        if complete {
            return Err(changed);
        }
        return Ok(cleanup);
    };
    if private
        .entries_bounded(2)
        .map_err(|_| changed)?
        .iter()
        .any(|name| {
            !matches!(
                name.to_str(),
                Some(PRIVATE_STAGE_NAME | PRIVATE_BACKUP_NAME)
            )
        })
    {
        return Err(changed);
    }
    let stage = checkpoint_directory(&private, PRIVATE_STAGE_NAME, record.stage)?;
    let backup = checkpoint_directory(&private, PRIVATE_BACKUP_NAME, record.backup)?;
    if complete && (stage.is_none() || backup.is_none()) {
        return Err(changed);
    }
    if backup.as_ref().is_some_and(|directory| {
        directory
            .entries_bounded(1)
            .map_or(true, |entries| !entries.is_empty())
    }) {
        return Err(changed);
    }
    let mut files = Vec::new();
    if let Some(stage) = &stage {
        let expected = record
            .payloads
            .iter()
            .filter(|_| record.published.is_empty())
            .map(|payload| (payload.name.as_str(), &payload.proof))
            .collect::<BTreeMap<_, _>>();
        let names = stage
            .entries_bounded(record.payloads.len().max(1))
            .map_err(|_| changed)?;
        if complete && names.len() != expected.len() {
            return Err(changed);
        }
        for name in names {
            let name = name.to_str().ok_or(changed)?;
            let proof = expected.get(name).ok_or(changed)?;
            files.push((name.to_string(), admit_checkpoint_file(stage, name, proof)?));
        }
    }
    cleanup.private = Some(private);
    cleanup.stage = stage;
    cleanup.backup = backup;
    cleanup.files = files;
    Ok(cleanup)
}

fn rollback_staging_checkpoint(
    root: &ManagedDir,
    checkpoint: &ManagedContentStagingCheckpoint,
) -> Result<(), ManagedContentCheckpointError> {
    let changed = ManagedContentCheckpointError::Changed;
    let cleanup = inspect_staging_checkpoint(root, checkpoint, false)?;
    for (parent, name, guard) in cleanup.published {
        parent
            .remove_guarded_file(&name, &guard)
            .map_err(|_| changed)?;
        #[cfg(test)]
        AFTER_PUBLISHED_FILE_REMOVAL.with(|hook| {
            if let Some(callback) = hook.take() {
                callback();
            }
        });
    }
    if let Some(private) = cleanup.private {
        if let Some(stage) = &cleanup.stage {
            for (name, guard) in cleanup.files {
                stage
                    .remove_guarded_file(&name, &guard)
                    .map_err(|_| changed)?;
            }
        }
        for (name, directory) in [
            (PRIVATE_STAGE_NAME, cleanup.stage),
            (PRIVATE_BACKUP_NAME, cleanup.backup),
        ] {
            let mut state =
                directory.map_or(CleanupDirectoryState::Done, CleanupDirectoryState::Known);
            advance_cleanup_directory(&private, name, &mut state);
            if !matches!(state, CleanupDirectoryState::Done) {
                return Err(changed);
            }
        }
        let mut state = CleanupDirectoryState::Known(private);
        advance_cleanup_directory(root, &checkpoint.record.private_name, &mut state);
        if !matches!(state, CleanupDirectoryState::Done) {
            return Err(changed);
        }
    }
    for mut created in cleanup.created_parents.into_iter().rev() {
        advance_cleanup_directory(&created.parent, created.name.as_str(), &mut created.cleanup);
        if !matches!(created.cleanup, CleanupDirectoryState::Done) {
            return Err(changed);
        }
        #[cfg(test)]
        AFTER_STAGING_PARENT_REMOVAL.with(|hook| {
            if let Some(callback) = hook.take() {
                callback();
            }
        });
    }
    let remaining = staging_public_bindings(root, checkpoint, false)?;
    if !remaining.created_parents.is_empty() || !remaining.published.is_empty() {
        return Err(changed);
    }
    if root
        .has_portably_exact_child_name(&checkpoint.record.private_name)
        .map_err(|_| changed)?
    {
        return Err(changed);
    }
    Ok(())
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

fn materialize_transaction_parents(state: &mut TransactionState) -> Result<(), ()> {
    let mut missing = state
        .mutations
        .iter()
        .filter_map(|mutation| match &mutation.parent {
            TransactionParent::Missing {
                path,
                first_missing,
            } if matches!(mutation.result, ManagedContentPathResult::Download(_)) => {
                Some((path.clone(), *first_missing))
            }
            TransactionParent::Missing { .. } => None,
            TransactionParent::Resolved { .. } => None,
        })
        .collect::<Vec<_>>();
    missing.sort_by(|(left, _), (right, _)| {
        left.as_str()
            .split('/')
            .count()
            .cmp(&right.as_str().split('/').count())
            .then_with(|| left.cmp(right))
    });
    missing.dedup();

    let mut resolved = BTreeMap::<String, ManagedDir>::new();
    for (path, first_missing) in missing {
        let mut parent = state.root.clone();
        let mut prefix = String::new();
        for (index, segment) in path.as_str().split('/').enumerate() {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            if let Some(known) = resolved.get(&prefix) {
                parent = known.clone();
                continue;
            }
            let child = if index < first_missing {
                parent
                    .open_child_if_exists(segment)
                    .map_err(|_| ())?
                    .ok_or(())?
            } else {
                let child = parent.create_child_new(segment).map_err(|_| ())?;
                state.created_parents.push(CreatedTransactionParent {
                    parent: parent.clone(),
                    name: PortableFileName::new_exact(segment)
                        .expect("validated parent component remains portable"),
                    cleanup: CleanupDirectoryState::Known(child.clone()),
                });
                child
            };
            resolved.insert(prefix.clone(), child.clone());
            parent = child;
        }
        resolve_materialized_parent(&mut state.mutations, &path, &parent);
        resolve_observed_parent(&mut state.read_preconditions, &path, &parent);
    }
    Ok(())
}

fn resolve_materialized_parent(
    mutations: &mut [TransactionMutation],
    path: &PortableRelativePath,
    directory: &ManagedDir,
) {
    for mutation in mutations {
        if matches!(&mutation.parent, TransactionParent::Missing { path: current, .. } if current == path)
        {
            mutation.parent = TransactionParent::Resolved {
                directory: directory.clone(),
            };
        }
    }
}

fn resolve_observed_parent(
    observations: &mut [PathObservationAuthority],
    path: &PortableRelativePath,
    directory: &ManagedDir,
) {
    for observation in observations {
        if matches!(&observation.parent, TransactionParent::Missing { path: current, .. } if current == path)
        {
            observation.parent = TransactionParent::Resolved {
                directory: directory.clone(),
            };
        }
    }
}

fn prepare_publication_prefix(state: &mut TransactionState) -> Result<(), ()> {
    if !revalidate_all(state) {
        return Err(());
    }
    if materialize_transaction_parents(state).is_err() {
        state.terminal_failure = ManagedContentTransactionFailure::ClaimFailed;
        return Err(());
    }
    if !revalidate_all(state) {
        return Err(());
    }
    Ok(())
}

fn drive_commit(
    mut state: TransactionState,
    before_manifest: impl FnOnce(&TransactionState) -> bool,
) -> ManagedContentTransactionOutcome {
    if prepare_publication_prefix(&mut state).is_err() {
        return drive_rollback(state, false);
    }
    for index in 0..state.mutations.len() {
        if state.mutations[index].old_guard.is_none() {
            continue;
        }
        let mutation = &mut state.mutations[index];
        let guard = mutation
            .old_guard
            .as_mut()
            .expect("exact observation has a guard");
        if mutation
            .parent
            .resolved()
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
        if !prior_guard_matches_observation(
            &state.backup,
            state.mutations[index].backup_name.as_str(),
            state.mutations[index]
                .old_guard
                .as_ref()
                .expect("claimed mutation retains its exact guard"),
            &state.mutations[index].observed,
        ) {
            state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    for index in 0..state.mutations.len() {
        let ManagedContentPathResult::Download(id) = &state.mutations[index].result else {
            continue;
        };
        let Some(payload_index) = state.staged_by_id.get(id).copied() else {
            return recovery(state, TransactionIntent::Fail);
        };
        let destination_parent = state.mutations[index].parent.resolved().clone();
        let destination_name = state.mutations[index].name.clone();
        let payload_name = state.payloads[payload_index].name.clone();
        if state
            .stage
            .rename_guarded_file_no_replace(
                payload_name.as_str(),
                state.payloads[payload_index]
                    .guard
                    .as_mut()
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
        if !state.mutations[index]
            .installed_guard
            .as_ref()
            .is_some_and(|guard| {
                payload_guard_matches_report(
                    &destination_parent,
                    destination_name.as_str(),
                    guard,
                    &state.payloads[payload_index].report,
                )
            })
        {
            state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    let mut synced = HashSet::new();
    for mutation in &state.mutations {
        if (mutation.claimed || mutation.installed)
            && synced.insert(mutation.parent.resolved().inner.identity)
            && mutation.parent.resolved().sync().is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::SyncFailed;
            return recovery(state, TransactionIntent::Fail);
        }
    }
    if !before_manifest(&state) {
        state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
        return drive_rollback(state, false);
    }
    #[cfg(any(test, feature = "test-support"))]
    if let Some(hook) = state.before_manifest_revalidation.take() {
        hook();
    }
    if !created_transaction_parent_bindings_match(&state)
        || !revalidate_transaction_logical_names(&state)
        || !revalidate_read_preconditions(&state)
        || !revalidate_final_effects(&state)
    {
        state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
        return drive_rollback(state, false);
    }
    if let Some(guard) = state.manifest.guard.as_mut() {
        if state
            .root
            .rename_guarded_file_no_replace(MANIFEST_NAME, guard, &state.backup, "manifest-old")
            .is_err()
        {
            state.terminal_failure = ManagedContentTransactionFailure::ManifestFailed;
            return recovery(state, TransactionIntent::Fail);
        }
        state.manifest_claimed = true;
        if !manifest_guard_matches_prior(
            &state.backup,
            "manifest-old",
            state
                .manifest
                .guard
                .as_ref()
                .expect("claimed manifest retains its exact guard"),
            state.manifest.bytes.as_deref(),
        ) {
            state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
            return recovery(state, TransactionIntent::Fail);
        }
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
    state.manifest_committed = state.manifest_installed.as_ref().is_some_and(|guard| {
        state
            .root
            .read_guarded_file_bounded(MANIFEST_NAME, guard, MAX_MANIFEST_BYTES as u64)
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

fn transaction_parent_directory(
    root: &ManagedDir,
    parent: &TransactionParent,
) -> Result<Option<ManagedDir>, ()> {
    match parent {
        TransactionParent::Resolved { directory, .. } => Ok(Some(directory.clone())),
        TransactionParent::Missing { path, .. } => {
            match resolve_transaction_parent(root, Some(path)) {
                Ok(TransactionParent::Resolved { directory, .. }) => Ok(Some(directory)),
                Ok(TransactionParent::Missing { .. }) => Ok(None),
                Err(_) => Err(()),
            }
        }
    }
}

fn classify_transaction_parent_name(
    root: &ManagedDir,
    parent: &TransactionParent,
    name: &str,
) -> ExactBindingState {
    match transaction_parent_directory(root, parent) {
        Ok(Some(directory)) => classify_name(&directory, name),
        Ok(None) => ExactBindingState::Absent,
        Err(()) => ExactBindingState::Unknown,
    }
}

fn classify_transaction_parent_file(
    root: &ManagedDir,
    parent: &TransactionParent,
    name: &str,
    guard: &ManagedFileGuard,
) -> ExactBindingState {
    match transaction_parent_directory(root, parent) {
        Ok(Some(directory)) => classify_exact_file(&directory, name, guard),
        Ok(None) => ExactBindingState::Absent,
        Err(()) => ExactBindingState::Unknown,
    }
}

fn inspect_transaction_parent_file(
    root: &ManagedDir,
    parent: &TransactionParent,
    name: &str,
) -> Result<Option<ManagedFileGuard>, ()> {
    match transaction_parent_directory(root, parent)? {
        Some(directory) => inspect_exact_file(&directory, name),
        None => Ok(None),
    }
}

fn reproject_transaction_parent_guard(
    root: &ManagedDir,
    parent: &TransactionParent,
    name: &str,
    guard: &mut ManagedFileGuard,
) -> Result<bool, ()> {
    match transaction_parent_directory(root, parent)? {
        Some(directory) => directory.reproject_guard_at(name, guard).map_err(|_| ()),
        None => Ok(false),
    }
}

fn inspect_exact_file(directory: &ManagedDir, name: &str) -> Result<Option<ManagedFileGuard>, ()> {
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
        && created_transaction_parent_bindings_match(state)
        && revalidate_transaction_logical_names(state)
        && state.mutations.iter().all(|mutation| {
            observed_parent_binding_matches(
                &state.root,
                &mutation.parent,
                mutation.name.as_str(),
                &mutation.old_guard,
            )
        })
        && revalidate_read_preconditions(state)
}

fn created_transaction_parent_bindings_match(state: &TransactionState) -> bool {
    state.created_parents.iter().all(|created| {
        created
            .parent
            .open_child_if_exists(created.name.as_str())
            .is_ok_and(|current| {
                current.is_some_and(|current| {
                    current.inner.identity
                        == match &created.cleanup {
                            CleanupDirectoryState::Known(directory) => directory.inner.identity,
                            CleanupDirectoryState::Discover | CleanupDirectoryState::Done => {
                                return false;
                            }
                        }
                })
            })
    })
}

fn revalidate_transaction_logical_names(state: &TransactionState) -> bool {
    validate_path_name_bindings(
        state.path_policy,
        state
            .mutations
            .iter()
            .filter_map(|mutation| {
                mutation
                    .parent
                    .directory()
                    .map(|parent| (parent, &mutation.name))
            })
            .chain(state.read_preconditions.iter().filter_map(|observation| {
                observation
                    .parent
                    .directory()
                    .map(|parent| (parent, &observation.name))
            })),
    )
    .is_ok()
}

fn observed_parent_binding_matches(
    root: &ManagedDir,
    parent: &TransactionParent,
    name: &str,
    guard: &Option<ManagedFileGuard>,
) -> bool {
    match parent {
        TransactionParent::Resolved { directory, .. } => {
            observed_binding_matches(directory, name, guard)
        }
        TransactionParent::Missing {
            path,
            first_missing,
        } => {
            guard.is_none()
                && matches!(
                    resolve_transaction_parent(root, Some(path)),
                    Ok(TransactionParent::Missing {
                        first_missing: current,
                        ..
                    }) if current == *first_missing
                )
        }
    }
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
        observed_parent_binding_matches(
            &state.root,
            &precondition.parent,
            precondition.name.as_str(),
            &precondition.guard,
        )
    })
}

fn revalidate_final_effects(state: &TransactionState) -> bool {
    state
        .mutations
        .iter()
        .all(|mutation| match &mutation.result {
            ManagedContentPathResult::Absent => {
                classify_transaction_parent_name(
                    &state.root,
                    &mutation.parent,
                    mutation.name.as_str(),
                ) == ExactBindingState::Absent
            }
            ManagedContentPathResult::Download(id) => {
                let Some(payload_index) = state.staged_by_id.get(id).copied() else {
                    return false;
                };
                mutation.installed_guard.as_ref().is_some_and(|guard| {
                    classify_exact_file(mutation.parent.resolved(), mutation.name.as_str(), guard)
                        == ExactBindingState::Exact
                        && payload_guard_matches_report(
                            mutation.parent.resolved(),
                            mutation.name.as_str(),
                            guard,
                            &state.payloads[payload_index].report,
                        )
                })
            }
        })
}

fn prior_guard_matches_observation(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    observed: &ManagedContentObservedState,
) -> bool {
    let ManagedContentObservedState::Exact { size, sha512 } = observed else {
        return false;
    };
    guard.size() == *size
        && directory
            .sha512_guarded_file(name, guard, MAX_CONTENT_FILE_BYTES)
            .is_ok_and(|digest| digest == sha512.as_ref())
}

fn manifest_guard_matches_prior(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    expected: Option<&[u8]>,
) -> bool {
    let Some(expected) = expected else {
        return false;
    };
    directory
        .read_guarded_file_bounded(name, guard, MAX_MANIFEST_BYTES as u64)
        .is_ok_and(|observed| observed == expected)
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
        let removal_failed = state.manifest.guard.as_ref().is_none_or(|guard| {
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
    finish_transaction_cleanup(state, TransactionIntent::Commit)
}

fn drive_rollback(
    mut state: TransactionState,
    cancelled: bool,
) -> ManagedContentTransactionOutcome {
    if state.manifest_committed {
        return cleanup_committed(state);
    }
    if let Some(guard) = state.manifest_installed.as_ref() {
        if state
            .root
            .remove_guarded_file(MANIFEST_NAME, guard)
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
        state.manifest_installed = None;
    }
    if state.manifest_claimed {
        let guard = state
            .manifest
            .guard
            .as_mut()
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
        if !manifest_guard_matches_prior(
            &state.root,
            MANIFEST_NAME,
            state
                .manifest
                .guard
                .as_ref()
                .expect("restored manifest retains its exact guard"),
            state.manifest.bytes.as_deref(),
        ) {
            state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
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
                .resolved()
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
            let mutation = &mut state.mutations[index];
            let guard = mutation
                .old_guard
                .as_mut()
                .expect("claimed mutation has an exact observation");
            if state
                .backup
                .rename_guarded_file_no_replace(
                    mutation.backup_name.as_str(),
                    guard,
                    mutation.parent.resolved(),
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
            if !prior_guard_matches_observation(
                mutation.parent.resolved(),
                mutation.name.as_str(),
                mutation
                    .old_guard
                    .as_ref()
                    .expect("restored mutation retains its exact guard"),
                &mutation.observed,
            ) {
                state.terminal_failure = ManagedContentTransactionFailure::ObservationDrift;
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
    finish_transaction_cleanup(
        state,
        if cancelled {
            TransactionIntent::Cancel
        } else {
            TransactionIntent::Fail
        },
    )
}

fn finish_transaction_cleanup(
    mut state: TransactionState,
    intent: TransactionIntent,
) -> ManagedContentTransactionOutcome {
    // Public effects are settled; only the retained exact cleanup capabilities remain.
    if state.root.inner.root.settle().is_err()
        || cleanup_private(&mut state).is_err()
        || (intent != TransactionIntent::Commit
            && cleanup_created_transaction_parents(&mut state).is_err())
    {
        state.terminal_failure = ManagedContentTransactionFailure::CleanupFailed;
        state.read_preconditions.clear();
        return ManagedContentTransactionOutcome::RecoveryRequired(ManagedContentRecovery {
            state: Some(RecoveryState::TransactionCleanup { state, intent }),
        });
    }
    let path_count = state.mutations.len();
    match intent {
        TransactionIntent::Commit => {
            ManagedContentTransactionOutcome::Committed(ManagedContentCommitReceipt {
                path_count,
                payload_count: state.payloads.len(),
            })
        }
        TransactionIntent::Cancel => {
            ManagedContentTransactionOutcome::Cancelled(ManagedContentCancelReceipt { path_count })
        }
        TransactionIntent::Fail => ManagedContentTransactionOutcome::Failed(state.terminal_failure),
    }
}

fn cleanup_created_transaction_parents(state: &mut TransactionState) -> Result<(), ()> {
    while let Some(created) = state.created_parents.last_mut() {
        advance_cleanup_directory(&created.parent, created.name.as_str(), &mut created.cleanup);
        if !matches!(created.cleanup, CleanupDirectoryState::Done) {
            return Err(());
        }
        state.created_parents.pop();
    }
    Ok(())
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
    StagingRollback {
        root: ManagedDir,
        authority: ManagedTransferAuthority,
        checkpoint: ManagedContentStagingCheckpoint,
    },
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
        retained: Vec<(
            ManagedContentPayloadId,
            TransferReport,
            ManagedTransferAuthority,
        )>,
        obligation: Option<TransientPublicationBatchObligation>,
    },
    StagePartial {
        transaction: TransactionState,
        retained: Vec<(
            ManagedContentPayloadId,
            TransferReport,
            ManagedTransferAuthority,
        )>,
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
    TransactionCleanup {
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
                stage: stage.map_or(
                    CleanupDirectoryState::Discover,
                    CleanupDirectoryState::Known,
                ),
                backup: backup.map_or(
                    CleanupDirectoryState::Discover,
                    CleanupDirectoryState::Known,
                ),
            }),
        }
    }

    pub fn reconcile(mut self) -> ManagedContentTransactionOutcome {
        match self
            .state
            .take()
            .expect("content recovery retains one exact state")
        {
            RecoveryState::StagingRollback {
                root,
                authority,
                checkpoint,
            } => {
                if root.settle().is_ok() && rollback_staging_checkpoint(&root, &checkpoint).is_ok()
                {
                    ManagedContentTransactionOutcome::Cancelled(ManagedContentCancelReceipt {
                        path_count: checkpoint.record.mutation_count,
                    })
                } else {
                    ManagedContentTransactionOutcome::RecoveryRequired(Self {
                        state: Some(RecoveryState::StagingRollback {
                            root,
                            authority,
                            checkpoint,
                        }),
                    })
                }
            }
            RecoveryState::Preparation {
                session,
                private_name,
            } => {
                if session.root.inner.root.settle().is_err() {
                    return ManagedContentTransactionOutcome::RecoveryRequired(Self::preparation(
                        session,
                        private_name,
                    ));
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
                    Err(_) => ManagedContentTransactionOutcome::RecoveryRequired(
                        Self::preparation(session, private_name),
                    ),
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
                if state.root.inner.root.settle().is_err() || !classify_transaction(&mut state) {
                    return recovery(state, intent);
                }
                if state.manifest_committed || intent == TransactionIntent::Commit {
                    cleanup_committed(state)
                } else {
                    drive_rollback(state, intent == TransactionIntent::Cancel)
                }
            }
            RecoveryState::TransactionCleanup { state, intent } => {
                finish_transaction_cleanup(state, intent)
            }
        }
    }
}

fn advance_cleanup_directory(parent: &ManagedDir, name: &str, state: &mut CleanupDirectoryState) {
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
        let Some(guard) = mutation.old_guard.as_mut() else {
            mutation.claimed = false;
            continue;
        };
        match mutation
            .parent
            .resolved()
            .reproject_guard_at(mutation.name.as_str(), guard)
        {
            Ok(true) => {}
            Ok(false) => {
                if state
                    .backup
                    .reproject_guard_at(mutation.backup_name.as_str(), guard)
                    .is_err()
                {
                    return false;
                }
            }
            Err(_) => return false,
        }
        let source = classify_exact_file(mutation.parent.resolved(), mutation.name.as_str(), guard);
        let backup = classify_exact_file(&state.backup, mutation.backup_name.as_str(), guard);
        match (source, backup) {
            (ExactBindingState::Exact, ExactBindingState::Absent)
                if prior_guard_matches_observation(
                    mutation.parent.resolved(),
                    mutation.name.as_str(),
                    guard,
                    &mutation.observed,
                ) =>
            {
                mutation.claimed = false;
            }
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Exact) => {
                if !prior_guard_matches_observation(
                    &state.backup,
                    mutation.backup_name.as_str(),
                    guard,
                    &mutation.observed,
                ) {
                    return false;
                }
                mutation.claimed = true;
            }
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Absent)
                if state.manifest_committed =>
            {
                mutation.claimed = false
            }
            _ => return false,
        }
    }

    if let Some(guard) = state.manifest.guard.as_mut() {
        match state.root.reproject_guard_at(MANIFEST_NAME, guard) {
            Ok(true) => {}
            Ok(false) => {
                if state
                    .backup
                    .reproject_guard_at("manifest-old", guard)
                    .is_err()
                {
                    return false;
                }
            }
            Err(_) => return false,
        }
        let source = classify_exact_file(&state.root, MANIFEST_NAME, guard);
        let backup = classify_exact_file(&state.backup, "manifest-old", guard);
        match (source, backup) {
            (ExactBindingState::Exact, ExactBindingState::Absent) => {
                if !manifest_guard_matches_prior(
                    &state.root,
                    MANIFEST_NAME,
                    guard,
                    state.manifest.bytes.as_deref(),
                ) {
                    return false;
                }
                state.manifest_claimed = false;
            }
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Exact) => {
                if !manifest_guard_matches_prior(
                    &state.backup,
                    "manifest-old",
                    guard,
                    state.manifest.bytes.as_deref(),
                ) {
                    return false;
                }
                state.manifest_claimed = true;
            }
            (ExactBindingState::Absent | ExactBindingState::Foreign, ExactBindingState::Absent)
                if state.manifest_committed =>
            {
                state.manifest_claimed = false
            }
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
                if (state.mutations[mutation_index].claimed || state.manifest_committed)
                    && classify_transaction_parent_name(
                        &state.root,
                        &state.mutations[mutation_index].parent,
                        state.mutations[mutation_index].name.as_str(),
                    ) != ExactBindingState::Absent
                {
                    return false;
                }
                continue;
            }
            ManagedContentPathResult::Download(id) => id,
        };
        let Some(payload_index) = state.staged_by_id.get(id).copied() else {
            return false;
        };
        let was_installed = state.mutations[mutation_index].installed_guard.is_some();
        let mut guard = state.mutations[mutation_index]
            .installed_guard
            .take()
            .or_else(|| state.payloads[payload_index].guard.take());
        let classified = (|| {
            if let Some(current) = guard.as_mut() {
                match state
                    .stage
                    .reproject_guard_at(state.payloads[payload_index].name.as_str(), current)
                {
                    Ok(true) => {}
                    Ok(false) => {
                        if reproject_transaction_parent_guard(
                            &state.root,
                            &state.mutations[mutation_index].parent,
                            state.mutations[mutation_index].name.as_str(),
                            current,
                        )
                        .is_err()
                        {
                            return false;
                        }
                    }
                    Err(_) => return false,
                }
            }
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
                    let installed = match inspect_transaction_parent_file(
                        &state.root,
                        &state.mutations[mutation_index].parent,
                        state.mutations[mutation_index].name.as_str(),
                    ) {
                        Ok(value) => value,
                        Err(()) => return false,
                    };
                    if let Some(installed) = installed {
                        if transaction_parent_directory(
                            &state.root,
                            &state.mutations[mutation_index].parent,
                        )
                        .is_ok_and(|parent| {
                            parent.is_some_and(|parent| {
                                payload_guard_matches_report(
                                    &parent,
                                    state.mutations[mutation_index].name.as_str(),
                                    &installed,
                                    &state.payloads[payload_index].report,
                                )
                            })
                        }) {
                            guard = Some(installed);
                        } else if !destination_matches_prior(state, mutation_index) {
                            return false;
                        }
                    }
                }
            }
            let Some(current) = guard.as_ref() else {
                if state.manifest_committed || !destination_matches_prior(state, mutation_index) {
                    return false;
                }
                state.mutations[mutation_index].installed = false;
                return true;
            };
            let staged = classify_exact_file(
                &state.stage,
                state.payloads[payload_index].name.as_str(),
                current,
            );
            let installed = classify_transaction_parent_file(
                &state.root,
                &state.mutations[mutation_index].parent,
                state.mutations[mutation_index].name.as_str(),
                current,
            );
            match (staged, installed) {
                (ExactBindingState::Exact, ExactBindingState::Absent) => {
                    if !payload_guard_matches_report(
                        &state.stage,
                        state.payloads[payload_index].name.as_str(),
                        current,
                        &state.payloads[payload_index].report,
                    ) {
                        return false;
                    }
                    state.payloads[payload_index].guard = guard.take();
                    state.mutations[mutation_index].installed = false;
                }
                (ExactBindingState::Exact, ExactBindingState::Foreign)
                    if destination_matches_prior(state, mutation_index) =>
                {
                    if !payload_guard_matches_report(
                        &state.stage,
                        state.payloads[payload_index].name.as_str(),
                        current,
                        &state.payloads[payload_index].report,
                    ) {
                        return false;
                    }
                    state.payloads[payload_index].guard = guard.take();
                    state.mutations[mutation_index].installed = false;
                }
                (ExactBindingState::Absent, ExactBindingState::Exact) => {
                    if !transaction_parent_directory(
                        &state.root,
                        &state.mutations[mutation_index].parent,
                    )
                    .is_ok_and(|parent| {
                        parent.is_some_and(|parent| {
                            payload_guard_matches_report(
                                &parent,
                                state.mutations[mutation_index].name.as_str(),
                                current,
                                &state.payloads[payload_index].report,
                            )
                        })
                    }) {
                        return false;
                    }
                    state.mutations[mutation_index].installed_guard = guard.take();
                    state.mutations[mutation_index].installed = true;
                }
                (ExactBindingState::Absent, ExactBindingState::Absent)
                    if !state.manifest_committed =>
                {
                    state.mutations[mutation_index].installed = false;
                }
                _ => return false,
            }
            true
        })();
        if !classified {
            if was_installed {
                state.mutations[mutation_index].installed_guard = guard;
            } else {
                state.payloads[payload_index].guard = guard;
            }
            return false;
        }
    }

    if state.manifest_publication_started && !state.manifest_committed {
        if let Some(guard) = state.manifest_installed.as_ref() {
            match classify_exact_file(&state.root, MANIFEST_NAME, guard) {
                ExactBindingState::Exact => {}
                ExactBindingState::Absent => state.manifest_installed = None,
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
                .read_guarded_file_bounded(MANIFEST_NAME, &guard, MAX_MANIFEST_BYTES as u64)
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
            classify_exact_file(mutation.parent.resolved(), mutation.name.as_str(), guard)
                == ExactBindingState::Exact
                && prior_guard_matches_observation(
                    mutation.parent.resolved(),
                    mutation.name.as_str(),
                    guard,
                    &mutation.observed,
                )
        }
        _ => {
            classify_transaction_parent_name(&state.root, &mutation.parent, mutation.name.as_str())
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
    (digests.sha1().is_some() || digests.sha512().is_some()) && sha1_matches && sha512_matches
}

fn map_stage_recovery(
    transaction: TransactionState,
    retained: Vec<(
        ManagedContentPayloadId,
        TransferReport,
        ManagedTransferAuthority,
    )>,
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
        TransientPublicationBatchOutcome::Partial { members, .. } => {
            recover_partial_stage(transaction, retained, members)
        }
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
    retained: Vec<(
        ManagedContentPayloadId,
        TransferReport,
        ManagedTransferAuthority,
    )>,
    members: Vec<TransientPublicationMember>,
) -> ManagedContentTransactionOutcome {
    let offset = transaction.payloads.len();
    let remaining = members
        .into_iter()
        .zip(retained)
        .enumerate()
        .map(
            |(local_index, (member, (id, report, authority)))| match member {
                TransientPublicationMember::Published(file) => StageRecoveryMember::Published {
                    index: offset + local_index,
                    id,
                    report,
                    authority,
                    file,
                },
                TransientPublicationMember::Unpublished(stage) => StageRecoveryMember::Unpublished(
                    VerifiedCreateOnly::from_content_stage(stage, report, authority),
                ),
            },
        )
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

    fn test_tempdir() -> std::io::Result<tempfile::TempDir> {
        // Root admission rejects symlink ancestry. Resolve only the ambient
        // temporary parent so fixtures use its physical macOS path.
        tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir())?)
    }

    fn content_root(
        temporary: &tempfile::TempDir,
    ) -> (super::super::ManagedTreeRoot, ManagedContentTransactionRoot) {
        let path = temporary.path();
        for child in ["mods", "resourcepacks", "shaderpacks"] {
            std::fs::create_dir_all(path.join(child)).expect("content parent");
        }
        reopen_content_root(path)
    }

    fn reopen_content_root(
        path: &std::path::Path,
    ) -> (super::super::ManagedTreeRoot, ManagedContentTransactionRoot) {
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

    fn deferred_absent_plan(
        session: &ManagedContentTransactionSession,
        path: PortableRelativePath,
    ) -> ManagedContentMutationPlan {
        ManagedContentMutationPlan::new_deferred(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path,
                ManagedContentObservedState::Absent,
                ManagedContentPathResult::Absent,
            )],
            Vec::new(),
            session.defer_manifest(),
        )
        .expect("deferred content plan")
    }

    fn download_plan(session: &ManagedContentTransactionSession) -> ManagedContentMutationPlan {
        let observations = session.observations();
        let mut mutations = Vec::with_capacity(observations.len());
        let mut payloads = Vec::with_capacity(observations.len());
        for (index, observation) in observations.iter().enumerate() {
            let id = ManagedContentPayloadId::new(&format!("payload-{index}")).expect("payload id");
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

    fn checkpoint_batch_fixture(
        temporary: &tempfile::TempDir,
        payload_count: usize,
        replace_existing: bool,
    ) -> (super::super::ManagedTreeRoot, ManagedContentTransferBatch) {
        let (tree, root) = content_root(temporary);
        if replace_existing {
            std::fs::write(temporary.path().join("mods/first.jar"), b"original").unwrap();
        }
        std::fs::write(temporary.path().join(MANIFEST_NAME), b"original manifest").unwrap();
        let mut paths = [
            "mods/first.jar",
            "mods/second.jar",
            "config/nested/options.txt",
        ]
        .map(|path| PortableRelativePath::new_exact(path).unwrap())
        .to_vec();
        paths.extend((3..payload_count).map(|index| {
            PortableRelativePath::new_exact(&format!("mods/member-{index}.jar")).unwrap()
        }));
        let session = transaction_session(root.for_pack(), paths);
        let observations = session.observations();
        let bytes = b"staged replacement";
        let mut mutations = Vec::new();
        let mut payloads = Vec::new();
        for (index, observed) in observations.iter().enumerate() {
            let id = ManagedContentPayloadId::new(&format!("member-{index}")).unwrap();
            let contract = TransferContract::authenticated_exact(
                std::num::NonZeroU64::new(bytes.len() as u64).unwrap(),
                crate::download::ExpectedTransferDigests::sha512(Sha512::digest(bytes).into()),
            )
            .unwrap();
            mutations.push(ManagedContentPathMutation::new(
                observed.path().clone(),
                observed.state().clone(),
                ManagedContentPathResult::Download(id.clone()),
            ));
            payloads.push(ManagedContentPayloadPlan::from_external_source(
                id, contract,
            ));
        }
        let manifest = session
            .bind_encoded_manifest(b"replacement manifest".to_vec())
            .unwrap();
        let plan =
            ManagedContentMutationPlan::new(&observations, mutations, payloads, manifest).unwrap();
        (tree, prepared(session, plan).into_transfer_batch())
    }

    fn advance_checkpoint_payload(
        batch: ManagedContentTransferBatch,
    ) -> ManagedContentTransferBatch {
        let ManagedContentTransferStep::Issued(issued) = batch.next() else {
            panic!("checkpoint fixture must retain a payload");
        };
        let (_sender, cancellation) = crate::download::transfer_cancellation_channel();
        match issued
            .copy_external(std::io::Cursor::new(b"staged replacement"), cancellation)
            .unwrap()
            .advance()
        {
            ManagedContentTransferAdvance::Continue(batch) => batch,
            _ => panic!("checkpoint payload must stage"),
        }
    }

    fn checkpoint_fixture(
        temporary: &tempfile::TempDir,
        replace_existing: bool,
    ) -> (
        super::super::ManagedTreeRoot,
        ManagedContentReadyTransaction,
    ) {
        let (tree, mut batch) = checkpoint_batch_fixture(temporary, 3, replace_existing);
        for _ in 0..3 {
            batch = advance_checkpoint_payload(batch);
        }
        let ManagedContentTransferStep::Complete(complete) = batch.next() else {
            panic!("checkpoint fixture must complete its transfers");
        };
        let ManagedContentStageOutcome::Ready(ready) = complete.stage() else {
            panic!("checkpoint fixture must become Ready");
        };
        (tree, ready)
    }

    #[test]
    fn committed_cleanup_recovers_after_foreign_stage_file_is_removed() {
        recover_private_cleanup(false);
    }

    #[test]
    fn cancelled_cleanup_recovers_after_foreign_stage_file_is_removed() {
        recover_private_cleanup(true);
    }

    fn recover_private_cleanup(cancelled: bool) {
        const CANARY_NAME: &str = "foreign-cleanup-canary";
        const CANARY: &[u8] = b"foreign file must remain";
        fn canary(private: &std::path::Path) -> std::path::PathBuf {
            let mut candidates = Vec::new();
            for entry in std::fs::read_dir(private).unwrap() {
                let entry = entry.unwrap();
                if entry.file_type().unwrap().is_dir() {
                    let candidate = entry.path().join(CANARY_NAME);
                    if candidate.try_exists().unwrap() {
                        assert!(std::fs::symlink_metadata(&candidate).unwrap().is_file());
                        assert_eq!(std::fs::read(&candidate).unwrap(), CANARY);
                        candidates.push(candidate);
                    }
                }
            }
            assert_eq!(candidates.len(), 1);
            candidates.pop().unwrap()
        }

        let temporary = test_tempdir().unwrap();
        let (tree, ready) = checkpoint_fixture(&temporary, true);
        let ManagedContentStageOutcome::Ready(ready) = ready.prepare_publication([23; 32]) else {
            panic!("fixture must prepare publication");
        };
        let private = ready.state.private.inner.path.clone();
        std::fs::write(ready.state.stage.inner.path.join(CANARY_NAME), CANARY).unwrap();
        let mut outcome = if cancelled {
            ready.cancel()
        } else {
            ready.commit()
        };
        for _ in 0..3 {
            let ManagedContentTransactionOutcome::RecoveryRequired(recovery) = outcome else {
                panic!("foreign stage file must retain cleanup");
            };
            canary(&private);
            outcome = recovery.reconcile();
        }
        let ManagedContentTransactionOutcome::RecoveryRequired(recovery) = outcome else {
            panic!("foreign stage file must remain an obligation");
        };
        std::fs::remove_file(canary(&private)).unwrap();
        let settled = recovery.reconcile();
        if cancelled {
            assert!(matches!(
                settled,
                ManagedContentTransactionOutcome::Cancelled(_)
            ));
            assert_eq!(
                std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
                b"original"
            );
            assert!(!temporary.path().join("mods/second.jar").exists());
            assert!(!temporary.path().join("config").exists());
            assert_eq!(
                std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                b"original manifest"
            );
        } else {
            assert!(matches!(
                settled,
                ManagedContentTransactionOutcome::Committed(_)
            ));
            for path in [
                "mods/first.jar",
                "mods/second.jar",
                "config/nested/options.txt",
            ] {
                assert_eq!(
                    std::fs::read(temporary.path().join(path)).unwrap(),
                    b"staged replacement"
                );
            }
            assert_eq!(
                std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                b"replacement manifest"
            );
        }
        assert!(!private.exists());
        tree.authority.root.settle().unwrap();
    }

    #[test]
    fn refused_recovery_keeps_installed_guard_until_exact_payload_returns() {
        let temporary = test_tempdir().unwrap();
        let (tree, mut ready) = checkpoint_fixture(&temporary, true);
        let destination = temporary.path().join("mods/first.jar");
        let retained = temporary.path().join("retained-payload");
        let moved_destination = destination.clone();
        let moved_retained = retained.clone();
        ready.state.before_manifest_revalidation = Some(Box::new(move || {
            std::fs::rename(&moved_destination, moved_retained).unwrap();
            std::fs::write(moved_destination, b"staged replacement").unwrap();
        }));
        let ManagedContentTransactionOutcome::RecoveryRequired(recovery) = ready.commit() else {
            panic!("foreign replacement must retain rollback");
        };
        let Some(RecoveryState::Transaction { state, .. }) = recovery.state.as_ref() else {
            panic!("public effects still require classification");
        };
        let mutation_index = state
            .mutations
            .iter()
            .position(|mutation| mutation.name.as_str() == "first.jar")
            .unwrap();
        let identity = state.mutations[mutation_index]
            .installed_guard
            .as_ref()
            .unwrap()
            .identity
            .clone();
        let ManagedContentTransactionOutcome::RecoveryRequired(recovery) = recovery.reconcile()
        else {
            panic!("foreign replacement cannot settle rollback");
        };
        let Some(RecoveryState::Transaction { state, .. }) = recovery.state.as_ref() else {
            panic!("public effects still require classification");
        };
        assert_eq!(
            state.mutations[mutation_index]
                .installed_guard
                .as_ref()
                .unwrap()
                .identity,
            identity
        );
        assert_eq!(std::fs::read(&destination).unwrap(), b"staged replacement");
        std::fs::remove_file(&destination).unwrap();
        std::fs::rename(retained, &destination).unwrap();
        assert!(matches!(
            recovery.reconcile(),
            ManagedContentTransactionOutcome::Failed(_)
        ));
        assert_eq!(std::fs::read(destination).unwrap(), b"original");
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
            b"original manifest"
        );
        tree.authority.root.settle().unwrap();
    }

    #[test]
    fn staging_checkpoint_records_materialized_parents_without_replacing_before_proofs() {
        let temporary = test_tempdir().unwrap();
        let (tree, mut ready) = checkpoint_fixture(&temporary, true);
        let initial = ready.checkpoint([12; 32]).unwrap().clone();
        assert!(!temporary.path().join("config").exists());
        assert_eq!(initial.record.directories.get("config"), Some(&None));
        assert!(!initial.record.directories.contains_key("config/nested"));
        let private = ready.state.private.inner.path.clone();

        let materialized = materialize_transaction_parents(&mut ready.state);
        let created_count = ready.state.created_parents.len();
        let parent_proofs = (|| -> Result<_, LoaderError> {
            let config = ready.state.root.open_child("config")?;
            let nested = config.open_child("nested")?;
            Ok(serde_json::json!({
                "config": config.inner.directory.incarnation_witness()?,
                "config/nested": nested.inner.directory.incarnation_witness()?,
            }))
        })();
        let payloads_still_private = ready.state.payloads.iter().all(|payload| {
            std::fs::read(private.join(PRIVATE_STAGE_NAME).join(payload.name.as_str()))
                .is_ok_and(|bytes| bytes == b"staged replacement")
        });
        let updated = ready.checkpoint([12; 32]).cloned();
        let outcome = ready.cancel();

        assert!(matches!(
            outcome,
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
        materialized.expect("the real parent publication boundary must be reached");
        assert_eq!(created_count, 2);
        assert!(payloads_still_private);
        assert!(!private.exists());
        assert!(!temporary.path().join("config").exists());
        assert!(!temporary.path().join("mods/second.jar").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
            b"original manifest"
        );
        drop(tree);

        let updated = updated.expect("parent-only publication must remain checkpointable");
        assert_eq!(
            serde_json::to_value(&updated.record.files).unwrap(),
            serde_json::to_value(&initial.record.files).unwrap()
        );
        assert_eq!(updated.record.directories, initial.record.directories);
        assert_eq!(
            serde_json::to_value(&updated.record.payloads).unwrap(),
            serde_json::to_value(&initial.record.payloads).unwrap()
        );
        let encoded = updated.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        let record: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(record["created_parents"], parent_proofs.unwrap());
    }

    #[test]
    fn staging_checkpoint_parent_codec_preserves_original_missing_boundaries() {
        let temporary = test_tempdir().unwrap();
        let (tree, mut ready) = checkpoint_fixture(&temporary, true);
        let initial = ready.checkpoint([13; 32]).unwrap().clone();
        let encoded = initial.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        assert!(!encoded.contains("created_parents"));
        ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
        let ManagedContentStageOutcome::Ready(mut ready) = ready.prepare_publication([13; 32])
        else {
            panic!("parent preparation must succeed");
        };
        let checkpoint = ready.checkpoint([13; 32]).unwrap().clone();
        let ManagedContentStageOutcome::Ready(mut ready) = ready.prepare_publication([13; 32])
        else {
            panic!("repeated parent preparation must retain its original proof");
        };
        let encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        assert_eq!(
            ready
                .checkpoint([13; 32])
                .unwrap()
                .encode(MAX_STAGING_CHECKPOINT_BYTES)
                .unwrap(),
            encoded
        );
        assert!(matches!(
            ready.cancel(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
        drop(tree);

        let record: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        let duplicate = encoded.replacen(
            "\"created_parents\":{",
            &format!(
                "\"created_parents\":{{\"config\":{},",
                record["created_parents"]["config"]
            ),
            1,
        );
        assert!(matches!(
            ManagedContentStagingCheckpoint::decode(&duplicate),
            Err(ManagedContentCheckpointError::Invalid)
        ));
        for change in [
            "original",
            "missing_ancestor",
            "alias",
            "unrelated",
            "union_alias",
        ] {
            let mut changed = record.clone();
            match change {
                "original" => {
                    changed["directories"]["config"] = changed["created_parents"]["config"].clone()
                }
                "missing_ancestor" => {
                    changed["created_parents"]
                        .as_object_mut()
                        .unwrap()
                        .remove("config");
                }
                "alias" => {
                    changed["created_parents"]["Config"] =
                        changed["created_parents"]["config"].clone()
                }
                "unrelated" => {
                    changed["created_parents"]["unrelated"] =
                        changed["created_parents"]["config"].clone()
                }
                "union_alias" => {
                    changed["files"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"path":"Config/other.txt","proof":null}));
                    changed["directories"]["Config"] = serde_json::Value::Null;
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    ManagedContentStagingCheckpoint::decode(&changed.to_string()),
                    Err(ManagedContentCheckpointError::Invalid)
                ),
                "{change}"
            );
        }
        // The previous checkpoint must not acquire the later parent authority.
        assert!(initial.record.created_parents.is_empty());
        assert_eq!(initial.record.directories.get("config"), Some(&None));
    }

    #[test]
    fn staging_checkpoint_parent_refusal_preserves_staged_and_foreign_objects() {
        for change in [
            "file",
            "hidden",
            "directory",
            "replaced",
            "unrecorded",
            "original_missing",
            "race",
        ] {
            let temporary = test_tempdir().unwrap();
            let (tree, ready) = checkpoint_fixture(&temporary, true);
            let ManagedContentStageOutcome::Ready(mut ready) = ready.prepare_publication([14; 32])
            else {
                panic!("parent preparation must succeed");
            };
            let checkpoint = ready.checkpoint([14; 32]).unwrap().clone();
            let private = ready.state.private.inner.path.clone();
            let config = temporary.path().join("config");
            match change {
                "file" => std::fs::write(config.join("nested/unowned"), b"keep").unwrap(),
                "hidden" => std::fs::write(config.join(".unowned"), b"keep").unwrap(),
                "directory" => std::fs::create_dir(config.join("unowned")).unwrap(),
                "replaced" => {
                    std::fs::rename(&config, temporary.path().join("displaced-config")).unwrap();
                    std::fs::create_dir_all(config.join("nested")).unwrap();
                }
                "unrecorded" => {
                    std::fs::create_dir(config.join("nested/extra")).unwrap();
                }
                "original_missing" => {
                    std::fs::remove_dir(config.join("nested")).unwrap();
                    std::fs::rename(
                        temporary.path().join("mods"),
                        temporary.path().join("displaced-mods"),
                    )
                    .unwrap();
                }
                "race" => {}
                _ => unreachable!(),
            }
            drop((ready, tree));
            let (tree, root) = reopen_content_root(temporary.path());
            let restored = root
                .for_pack()
                .restore_staging_checkpoint(checkpoint, [14; 32]);
            let retained = if change == "race" {
                let recovery = restored.unwrap();
                std::fs::write(config.join("nested/unowned"), b"keep").unwrap();
                let outcome = recovery.reconcile();
                assert!(matches!(
                    outcome,
                    ManagedContentTransactionOutcome::RecoveryRequired(_)
                ));
                Some(outcome)
            } else {
                assert!(
                    matches!(restored, Err(ManagedContentCheckpointError::Changed)),
                    "{change}"
                );
                None
            };
            for index in 0..3 {
                assert_eq!(
                    std::fs::read(private.join(format!("stage/payload-{index}"))).unwrap(),
                    b"staged replacement",
                    "{change}"
                );
            }
            let original = if change == "original_missing" {
                "displaced-mods/first.jar"
            } else {
                "mods/first.jar"
            };
            assert_eq!(
                std::fs::read(temporary.path().join(original)).unwrap(),
                b"original"
            );
            assert_eq!(
                std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                b"original manifest"
            );
            match change {
                "file" | "race" => assert_eq!(
                    std::fs::read(config.join("nested/unowned")).unwrap(),
                    b"keep"
                ),
                "hidden" => assert_eq!(std::fs::read(config.join(".unowned")).unwrap(), b"keep"),
                "directory" => assert!(config.join("unowned").is_dir()),
                "unrecorded" => assert!(config.join("nested/extra").is_dir()),
                "replaced" => {
                    assert!(config.join("nested").is_dir());
                    assert!(temporary.path().join("displaced-config/nested").is_dir());
                }
                "original_missing" => assert!(!temporary.path().join("mods").exists()),
                _ => unreachable!(),
            }
            drop((retained, tree));
        }
    }

    #[test]
    fn staging_checkpoint_parent_preparation_retains_optional_refusal_and_binding_fence() {
        for refusal in [
            ManagedContentCheckpointError::Unsupported,
            ManagedContentCheckpointError::Capacity,
        ] {
            let temporary = test_tempdir().unwrap();
            let (tree, mut ready) = checkpoint_fixture(&temporary, false);
            ready.checkpoint([16; 32]).unwrap();
            // Exercise an already classified optional refusal without assuming
            // this fixture filesystem lacks birth-time support.
            ready.state.checkpoint.as_mut().unwrap().checkpoint = Err(refusal);
            for _ in 0..2 {
                let ManagedContentStageOutcome::Ready(prepared) =
                    ready.prepare_publication([16; 32])
                else {
                    panic!("an optional checkpoint refusal must not reduce live admission");
                };
                ready = prepared;
                assert!(matches!(ready.checkpoint([16; 32]), Err(error) if error == refusal));
                assert_eq!(ready.state.created_parents.len(), 2);
            }
            assert!(matches!(
                ready.commit_with_checkpoint([16; 32], |_| panic!("optional refusal has no proof")),
                ManagedContentTransactionOutcome::Committed(_)
            ));
            assert_eq!(
                std::fs::read(temporary.path().join("config/nested/options.txt")).unwrap(),
                b"staged replacement"
            );
            drop(tree);
        }
        let temporary = test_tempdir().unwrap();
        let (tree, ready) = checkpoint_fixture(&temporary, true);
        let ManagedContentStageOutcome::Ready(ready) = ready.prepare_publication([16; 32]) else {
            panic!("parent preparation must succeed");
        };
        assert!(temporary.path().join("config/nested").is_dir());
        assert!(matches!(
            ready.commit_with_checkpoint([17; 32], |_| panic!("wrong binding cannot publish")),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert!(!temporary.path().join("config").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
            b"original"
        );
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
            b"original manifest"
        );
        drop(tree);
    }

    #[test]
    fn staging_checkpoint_parent_cleanup_survives_two_process_exits() {
        const FIXTURE: &str = "AXIAL_CONTENT_PARENT_CHECKPOINT_FIXTURE";
        const PHASE: &str = "AXIAL_CONTENT_PARENT_CHECKPOINT_PHASE";
        if let Some(container) = std::env::var_os(FIXTURE) {
            let container = std::path::PathBuf::from(container);
            if std::env::var(PHASE).unwrap() == "stage" {
                let temporary = tempfile::tempdir_in(&container).unwrap();
                let (_tree, ready) = checkpoint_fixture(&temporary, true);
                let ManagedContentStageOutcome::Ready(mut ready) =
                    ready.prepare_publication([15; 32])
                else {
                    panic!("parent preparation must succeed");
                };
                let encoded = ready
                    .checkpoint([15; 32])
                    .unwrap()
                    .encode(MAX_STAGING_CHECKPOINT_BYTES)
                    .unwrap();
                assert!(temporary.path().join("config/nested").is_dir());
                assert!(!temporary.path().join("config/nested/options.txt").exists());
                std::fs::write(
                    container.join("root-path"),
                    temporary.path().to_str().unwrap(),
                )
                .unwrap();
                std::fs::write(container.join("checkpoint"), encoded).unwrap();
                std::process::exit(42);
            }
            let path = std::path::PathBuf::from(
                std::fs::read_to_string(container.join("root-path")).unwrap(),
            );
            let encoded = std::fs::read_to_string(container.join("checkpoint")).unwrap();
            let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
            let (_tree, root) = reopen_content_root(&path);
            let recovery = root
                .for_pack()
                .restore_staging_checkpoint(checkpoint, [15; 32])
                .unwrap();
            AFTER_STAGING_PARENT_REMOVAL.with(|hook| hook.set(Some(|| std::process::exit(43))));
            let _outcome = recovery.reconcile();
            panic!("cleanup must exit after an actual recorded parent removal");
        }

        let container = test_tempdir().unwrap();
        for (phase, code) in [("stage", 42), ("cleanup", 43)] {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "managed_fs::content_transaction::tests::staging_checkpoint_parent_cleanup_survives_two_process_exits", "--nocapture"])
                .env(FIXTURE, container.path())
                .env(PHASE, phase)
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn().unwrap();
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            let timed_out = child.try_wait().unwrap().is_none();
            if timed_out {
                child.kill().unwrap();
            }
            let output = child.wait_with_output().unwrap();
            assert!(
                !timed_out && output.status.code() == Some(code),
                "{phase}: timed_out={timed_out}, status={:?}; {}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let path = std::path::PathBuf::from(
            std::fs::read_to_string(container.path().join("root-path")).unwrap(),
        );
        let encoded = std::fs::read_to_string(container.path().join("checkpoint")).unwrap();
        let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
        let private = path.join(&checkpoint.record.private_name);
        assert!(!private.exists());
        assert!(!path.join("config/nested").exists());
        assert_eq!(std::fs::read_dir(path.join("config")).unwrap().count(), 0);
        for _ in 0..2 {
            let (tree, root) = reopen_content_root(&path);
            let outcome = root
                .for_pack()
                .restore_staging_checkpoint(checkpoint.clone(), [15; 32])
                .unwrap()
                .reconcile();
            assert!(matches!(
                outcome,
                ManagedContentTransactionOutcome::Cancelled(_)
            ));
            assert!(!path.join("config").exists());
            assert!(!path.join("mods/second.jar").exists());
            assert_eq!(
                std::fs::read(path.join("mods/first.jar")).unwrap(),
                b"original"
            );
            assert_eq!(
                std::fs::read(path.join(MANIFEST_NAME)).unwrap(),
                b"original manifest"
            );
            drop(tree);
        }
        assert_eq!(
            std::fs::read_to_string(container.path().join("checkpoint")).unwrap(),
            encoded
        );
    }

    #[test]
    fn staging_checkpoint_preserves_post_move_pre_manifest_process_exit() {
        const FIXTURE: &str = "AXIAL_CONTENT_PUBLIC_MOVE_FIXTURE";
        fn snapshot(root: &std::path::Path, private: &str) -> BTreeMap<String, Option<Vec<u8>>> {
            let mut entries = BTreeMap::new();
            let mut pending = vec![
                "mods".to_string(),
                "config".to_string(),
                private.to_string(),
                MANIFEST_NAME.to_string(),
            ];
            while let Some(relative) = pending.pop() {
                assert!(entries.len() < 16);
                let path = root.join(&relative);
                let metadata = std::fs::symlink_metadata(&path).unwrap();
                let bytes = if metadata.is_dir() {
                    for child in std::fs::read_dir(&path).unwrap() {
                        assert!(pending.len() < 16);
                        pending.push(format!(
                            "{relative}/{}",
                            child.unwrap().file_name().to_str().unwrap()
                        ));
                    }
                    None
                } else {
                    assert!(metadata.is_file() && metadata.len() <= 64);
                    Some(std::fs::read(path).unwrap())
                };
                assert!(entries.insert(relative, bytes).is_none());
            }
            entries
        }

        if let Some(container) = std::env::var_os(FIXTURE) {
            let container = std::path::PathBuf::from(container);
            let temporary = tempfile::tempdir_in(&container).unwrap();
            let (_tree, ready) = checkpoint_fixture(&temporary, true);
            let ManagedContentStageOutcome::Ready(mut ready) = ready.prepare_publication([18; 32])
            else {
                panic!("parent preparation must succeed");
            };
            let encoded = ready
                .checkpoint([18; 32])
                .unwrap()
                .encode(MAX_STAGING_CHECKPOINT_BYTES)
                .unwrap();
            let original = ready
                .state
                .mutations
                .iter()
                .find(|mutation| mutation.old_guard.is_some())
                .unwrap();
            let original_identity = original.old_guard.as_ref().unwrap().identity();
            let backup_name = original.backup_name.clone();
            let backup = ready.state.backup.clone();
            let replacements = ready
                .state
                .mutations
                .iter()
                .map(|mutation| {
                    let ManagedContentPathResult::Download(id) = &mutation.result else {
                        panic!("fixture download");
                    };
                    let payload = &ready.state.payloads[ready.state.staged_by_id[id]];
                    (
                        mutation.parent.resolved().clone(),
                        mutation.name.clone(),
                        payload.guard.as_ref().unwrap().identity(),
                    )
                })
                .collect::<Vec<_>>();
            let root = temporary.path().to_path_buf();
            let private = ready.state.private_name.as_str().to_string();
            std::fs::write(container.join("root-path"), root.to_str().unwrap()).unwrap();
            std::fs::write(container.join("checkpoint"), encoded).unwrap();
            ready.state.before_manifest_revalidation = Some(Box::new(move || {
                let original = backup
                    .inspect_regular_file(backup_name.as_str())
                    .unwrap()
                    .unwrap();
                assert_eq!(original.identity(), original_identity);
                for (parent, name, identity) in replacements {
                    let installed = parent.inspect_regular_file(name.as_str()).unwrap().unwrap();
                    assert_eq!(installed.identity(), identity);
                }
                let mut expected = BTreeMap::new();
                for directory in [
                    "mods".to_string(),
                    "config".to_string(),
                    "config/nested".to_string(),
                    private.clone(),
                    format!("{private}/stage"),
                    format!("{private}/backup"),
                ] {
                    expected.insert(directory, None);
                }
                for path in [
                    "mods/first.jar",
                    "mods/second.jar",
                    "config/nested/options.txt",
                ] {
                    expected.insert(path.to_string(), Some(b"staged replacement".to_vec()));
                }
                expected.insert(
                    MANIFEST_NAME.to_string(),
                    Some(b"original manifest".to_vec()),
                );
                expected.insert(
                    format!("{private}/backup/{}", backup_name.as_str()),
                    Some(b"original".to_vec()),
                );
                let observed = snapshot(&root, &private);
                assert_eq!(observed, expected);
                std::fs::write(
                    container.join("publication-witness"),
                    serde_json::to_vec(&observed).unwrap(),
                )
                .unwrap();
                std::process::exit(42);
            }));
            let _outcome = ready.commit_with_checkpoint([18; 32], |_| {
                panic!("replacement transactions have no create-only proof")
            });
            panic!("production commit must reach the pre-manifest hard exit");
        }

        let container = test_tempdir().unwrap();
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "managed_fs::content_transaction::tests::staging_checkpoint_preserves_post_move_pre_manifest_process_exit", "--nocapture"])
            .env(FIXTURE, container.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out && output.status.code() == Some(42),
            "timed_out={timed_out}, status={:?}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let root = std::path::PathBuf::from(
            std::fs::read_to_string(container.path().join("root-path")).unwrap(),
        );
        let encoded = std::fs::read_to_string(container.path().join("checkpoint")).unwrap();
        let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
        let witness: BTreeMap<String, Option<Vec<u8>>> = serde_json::from_slice(
            &std::fs::read(container.path().join("publication-witness")).unwrap(),
        )
        .unwrap();
        assert_eq!(snapshot(&root, &checkpoint.record.private_name), witness);
        for _ in 0..2 {
            let (tree, admitted) = reopen_content_root(&root);
            assert!(matches!(
                admitted
                    .for_pack()
                    .restore_staging_checkpoint(checkpoint.clone(), [18; 32]),
                Err(ManagedContentCheckpointError::Changed)
            ));
            drop(tree);
            assert_eq!(snapshot(&root, &checkpoint.record.private_name), witness);
            assert_eq!(
                std::fs::read_to_string(container.path().join("checkpoint")).unwrap(),
                encoded
            );
        }
    }

    #[test]
    fn published_checkpoint_compensates_create_only_process_exit() {
        const FIXTURE: &str = "AXIAL_CONTENT_ADDITIONS_FIXTURE";
        const BINDING: [u8; 32] = [19; 32];
        if let Some(container) = std::env::var_os(FIXTURE) {
            let container = std::path::PathBuf::from(container);
            if std::env::var_os("AXIAL_CONTENT_ADDITIONS_CLEANUP").is_some() {
                let root = std::path::PathBuf::from(
                    std::fs::read_to_string(container.join("root-path")).unwrap(),
                );
                let checkpoint = ManagedContentStagingCheckpoint::decode(
                    &std::fs::read_to_string(container.join("checkpoint")).unwrap(),
                )
                .unwrap();
                let (_tree, admitted) = reopen_content_root(&root);
                let recovery = admitted
                    .for_pack()
                    .restore_staging_checkpoint(checkpoint, BINDING)
                    .unwrap();
                AFTER_PUBLISHED_FILE_REMOVAL.with(|hook| {
                    hook.set(Some(|| std::process::exit(43)));
                });
                let _outcome = recovery.reconcile();
                panic!("cleanup must exit after an actual guarded public file removal");
            }
            let temporary = tempfile::tempdir_in(&container).unwrap();
            let (_tree, ready) = checkpoint_fixture(&temporary, false);
            std::fs::write(temporary.path().join("mods/keep.jar"), b"keep original").unwrap();
            let ManagedContentStageOutcome::Ready(mut ready) = ready.prepare_publication(BINDING)
            else {
                panic!("fixture must prepare publication");
            };
            std::fs::write(
                container.join("checkpoint"),
                ready
                    .checkpoint(BINDING)
                    .unwrap()
                    .encode(MAX_STAGING_CHECKPOINT_BYTES)
                    .unwrap(),
            )
            .unwrap();
            std::fs::copy(
                container.join("checkpoint"),
                container.join("before-checkpoint"),
            )
            .unwrap();
            let root = temporary.path().to_path_buf();
            std::fs::write(container.join("root-path"), root.to_str().unwrap()).unwrap();
            ready.state.before_manifest_revalidation = Some(Box::new(move || {
                for path in [
                    "mods/first.jar",
                    "mods/second.jar",
                    "config/nested/options.txt",
                ] {
                    assert_eq!(
                        std::fs::read(root.join(path)).unwrap(),
                        b"staged replacement"
                    );
                }
                assert_eq!(
                    std::fs::read(root.join(MANIFEST_NAME)).unwrap(),
                    b"original manifest"
                );
                std::process::exit(42);
            }));
            let _outcome = ready.commit_with_checkpoint(BINDING, |checkpoint| {
                std::fs::write(
                    container.join("checkpoint"),
                    checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap(),
                )
                .unwrap();
                true
            });
            panic!("production commit must reach the pre-manifest hard exit");
        }
        for cleanup in ["complete", "public_file_removed", "private_removed"] {
            let container = test_tempdir().unwrap();
            let (root, checkpoint) = published_checkpoint_fixture(&container);
            match cleanup {
                "public_file_removed" => {
                    run_published_checkpoint_child(container.path(), true);
                    assert_eq!(
                        checkpoint
                            .record
                            .published
                            .iter()
                            .filter(|file| !root.join(&file.path).exists())
                            .count(),
                        1
                    );
                }
                "private_removed" => {
                    let private = root.join(&checkpoint.record.private_name);
                    std::fs::remove_dir(private.join(PRIVATE_STAGE_NAME)).unwrap();
                    std::fs::remove_dir(private.join(PRIVATE_BACKUP_NAME)).unwrap();
                    std::fs::remove_dir(private).unwrap();
                    assert!(
                        checkpoint
                            .record
                            .published
                            .iter()
                            .all(|file| root.join(&file.path).is_file())
                    );
                }
                _ => {}
            }
            for _ in 0..2 {
                let (tree, admitted) = reopen_content_root(&root);
                let recovery = admitted
                    .for_pack()
                    .restore_staging_checkpoint(checkpoint.clone(), BINDING)
                    .expect("recorded create-only publication must remain compensatable");
                assert!(matches!(
                    recovery.reconcile(),
                    ManagedContentTransactionOutcome::Cancelled(_)
                ));
                assert!(!root.join("mods/first.jar").exists());
                assert!(!root.join("mods/second.jar").exists());
                assert!(!root.join("config").exists());
                assert!(!root.join(&checkpoint.record.private_name).exists());
                assert_eq!(
                    std::fs::read(root.join("mods/keep.jar")).unwrap(),
                    b"keep original"
                );
                assert_eq!(
                    std::fs::read(root.join(MANIFEST_NAME)).unwrap(),
                    b"original manifest"
                );
                drop(tree);
            }
        }
    }

    fn run_published_checkpoint_child(container: &std::path::Path, cleanup: bool) {
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "managed_fs::content_transaction::tests::published_checkpoint_compensates_create_only_process_exit", "--nocapture"])
            .env("AXIAL_CONTENT_ADDITIONS_FIXTURE", container)
            .envs(cleanup.then_some(("AXIAL_CONTENT_ADDITIONS_CLEANUP", "1")))
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let timed_out = child.try_wait().unwrap().is_none();
        if timed_out {
            child.kill().unwrap();
        }
        let output = child.wait_with_output().unwrap();
        assert!(
            !timed_out && output.status.code() == Some(if cleanup { 43 } else { 42 }),
            "timed_out={timed_out}, status={:?}; {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    fn published_checkpoint_fixture(
        container: &tempfile::TempDir,
    ) -> (std::path::PathBuf, ManagedContentStagingCheckpoint) {
        run_published_checkpoint_child(container.path(), false);
        let root = std::path::PathBuf::from(
            std::fs::read_to_string(container.path().join("root-path")).unwrap(),
        );
        let encoded = std::fs::read_to_string(container.path().join("checkpoint")).unwrap();
        let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
        assert_eq!(checkpoint.record.published.len(), 3);
        let before: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(container.path().join("before-checkpoint")).unwrap(),
        )
        .unwrap();
        let after: serde_json::Value = serde_json::from_str(&encoded).unwrap();
        for field in ["files", "payloads", "directories", "created_parents"] {
            assert_eq!(before[field], after[field], "{field}");
        }
        (root, checkpoint)
    }

    #[test]
    fn published_checkpoint_supports_direct_ready_commit_with_new_parents() {
        let temporary = test_tempdir().unwrap();
        let (_tree, ready) = checkpoint_fixture(&temporary, false);
        let mut published = false;
        let outcome = ready.commit_with_checkpoint([20; 32], |checkpoint| {
            published = checkpoint.record.published.len() == 3;
            true
        });
        assert!(
            matches!(outcome, ManagedContentTransactionOutcome::Committed(_)),
            "{outcome:?}"
        );
        assert!(published);
        assert_eq!(
            std::fs::read(temporary.path().join("config/nested/options.txt")).unwrap(),
            b"staged replacement"
        );
    }

    #[test]
    fn published_checkpoint_callback_refusal_and_drift_do_not_commit() {
        for drift in [false, true] {
            let temporary = test_tempdir().unwrap();
            let (_tree, ready) = checkpoint_fixture(&temporary, false);
            let path = temporary.path().join("config/nested/options.txt");
            let mut called = false;
            let outcome = ready.commit_with_checkpoint([21; 32], |checkpoint| {
                assert_eq!(checkpoint.record.published.len(), 3);
                called = true;
                if drift {
                    std::fs::write(&path, b"external change").unwrap();
                }
                drift
            });
            assert!(called);
            if drift {
                let ManagedContentTransactionOutcome::RecoveryRequired(recovery) = outcome else {
                    panic!("callback drift must retain exact recovery");
                };
                assert!(matches!(
                    recovery.reconcile(),
                    ManagedContentTransactionOutcome::RecoveryRequired(_)
                ));
                assert_eq!(std::fs::read(path).unwrap(), b"external change");
            } else {
                assert!(matches!(
                    outcome,
                    ManagedContentTransactionOutcome::Failed(
                        ManagedContentTransactionFailure::ObservationDrift
                    )
                ));
                assert!(!temporary.path().join("mods/first.jar").exists());
                assert!(!temporary.path().join("mods/second.jar").exists());
                assert!(!temporary.path().join("config").exists());
            }
            assert_eq!(
                std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                b"original manifest"
            );
        }
    }

    #[test]
    fn published_checkpoint_refuses_foreign_objects_before_any_cleanup() {
        for change in [
            "same_bytes",
            "rewrite",
            "alias",
            "stage_file",
            "backup_file",
            "private_file",
            "created_parent_file",
            "private_absent_foreign",
        ] {
            let container = test_tempdir().unwrap();
            let (root, checkpoint) = published_checkpoint_fixture(&container);
            let private = root.join(&checkpoint.record.private_name);
            let mut paths = [
                "mods/first.jar",
                "mods/second.jar",
                "config/nested/options.txt",
            ]
            .map(|path| root.join(path));
            let mut foreign = None;
            match change {
                "same_bytes" | "private_absent_foreign" => {
                    std::fs::rename(&paths[2], root.join("retained-payload")).unwrap();
                    std::fs::write(&paths[2], b"staged replacement").unwrap();
                    if change == "private_absent_foreign" {
                        std::fs::remove_dir(private.join(PRIVATE_STAGE_NAME)).unwrap();
                        std::fs::remove_dir(private.join(PRIVATE_BACKUP_NAME)).unwrap();
                        std::fs::remove_dir(&private).unwrap();
                    }
                }
                "rewrite" => {
                    let previous = std::fs::metadata(&paths[2]).unwrap().modified().unwrap();
                    std::fs::write(&paths[2], b"staged replacement").unwrap();
                    std::fs::File::options()
                        .write(true)
                        .open(&paths[2])
                        .unwrap()
                        .set_times(
                            std::fs::FileTimes::new()
                                .set_modified(previous + std::time::Duration::from_secs(86_400)),
                        )
                        .unwrap();
                    assert_ne!(
                        std::fs::metadata(&paths[2]).unwrap().modified().unwrap(),
                        previous
                    );
                }
                "alias" => {
                    let alias = paths[2].with_file_name("OPTIONS.txt");
                    std::fs::rename(&paths[2], &alias).unwrap();
                    paths[2] = alias;
                }
                _ => {
                    let path = match change {
                        "stage_file" => private.join("stage/payload-0"),
                        "backup_file" => private.join("backup/foreign"),
                        "private_file" => private.join("foreign"),
                        "created_parent_file" => root.join("config/nested/foreign"),
                        _ => unreachable!(),
                    };
                    std::fs::write(&path, b"foreign keep").unwrap();
                    foreign = Some(path);
                }
            }
            for _ in 0..2 {
                let (tree, admitted) = reopen_content_root(&root);
                assert!(
                    matches!(
                        admitted
                            .for_pack()
                            .restore_staging_checkpoint(checkpoint.clone(), [19; 32]),
                        Err(ManagedContentCheckpointError::Changed)
                    ),
                    "{change}"
                );
                for path in &paths {
                    assert_eq!(
                        std::fs::read(path).unwrap(),
                        b"staged replacement",
                        "{change}"
                    );
                }
                if let Some(path) = &foreign {
                    assert_eq!(std::fs::read(path).unwrap(), b"foreign keep");
                }
                if matches!(change, "same_bytes" | "private_absent_foreign") {
                    assert_eq!(
                        std::fs::read(root.join("retained-payload")).unwrap(),
                        b"staged replacement"
                    );
                }
                assert_eq!(
                    std::fs::read(root.join(MANIFEST_NAME)).unwrap(),
                    b"original manifest"
                );
                assert_eq!(
                    std::fs::read(root.join("mods/keep.jar")).unwrap(),
                    b"keep original"
                );
                drop(tree);
            }
        }
    }

    #[test]
    fn published_checkpoint_codec_requires_complete_distinct_mutation_mapping() {
        let container = test_tempdir().unwrap();
        let (_root, checkpoint) = published_checkpoint_fixture(&container);
        let encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        for change in ["missing", "duplicate", "read_precondition"] {
            let mut value: serde_json::Value = serde_json::from_str(&encoded).unwrap();
            match change {
                "missing" => {
                    value["published"].as_array_mut().unwrap().pop();
                }
                "duplicate" => {
                    value["published"][1]["payload_index"] =
                        value["published"][0]["payload_index"].clone();
                }
                "read_precondition" => {
                    value["files"]
                        .as_array_mut()
                        .unwrap()
                        .push(serde_json::json!({"path": "mods/unmutated.jar", "proof": null}));
                    value["published"][0]["path"] = serde_json::json!("mods/unmutated.jar");
                }
                _ => unreachable!(),
            }
            assert!(
                matches!(
                    ManagedContentStagingCheckpoint::decode(
                        &serde_json::to_string(&value).unwrap()
                    ),
                    Err(ManagedContentCheckpointError::Invalid)
                ),
                "{change}"
            );
        }
    }

    #[test]
    fn staging_checkpoint_prefix_reopens_with_reserved_slots_and_preserves_foreign_files() {
        for completed in [0, 512] {
            let temporary = test_tempdir().unwrap();
            let (tree, mut batch) = checkpoint_batch_fixture(&temporary, 513, true);
            let mut encoded = batch
                .checkpoint([6; 32])
                .unwrap()
                .unwrap()
                .encode(MAX_STAGING_CHECKPOINT_BYTES)
                .unwrap();
            for _ in 0..completed {
                batch = advance_checkpoint_payload(batch);
                if let Some(checkpoint) = batch.checkpoint([6; 32]).unwrap() {
                    encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
                }
            }
            assert!(batch.verified.is_empty());
            assert_eq!(batch.state.payloads.len(), completed);
            assert!(!batch.remaining.is_empty());
            let private = batch.state.private.inner.path.clone();
            let stage = private.join(PRIVATE_STAGE_NAME);
            assert_eq!(std::fs::read_dir(&stage).unwrap().count(), completed);
            let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
            assert_eq!(checkpoint.record.mutation_count, 513);
            assert_eq!(checkpoint.record.files.len(), 514);
            assert_eq!(checkpoint.record.payloads.len(), completed);
            if completed != 0 {
                std::fs::write(stage.join("unowned"), b"keep").unwrap();
            }
            // This in-process reopen settles only unstarted reservations; the
            // application subprocess fixture separately exercises hard exit.
            for slot in batch.remaining.drain(..) {
                assert!(matches!(
                    advance_transfer_unwind(TransferUnwindMember::Unstarted(slot)),
                    TransferUnwindAdvance::Settled
                ));
            }
            assert_eq!(
                std::fs::read_dir(&stage).unwrap().count(),
                completed + usize::from(completed != 0)
            );
            drop((batch, tree));
            if completed != 0 {
                let (tree, root) = content_root(&temporary);
                assert!(matches!(
                    root.for_pack()
                        .restore_staging_checkpoint(checkpoint.clone(), [6; 32]),
                    Err(ManagedContentCheckpointError::Changed)
                ));
                assert_eq!(std::fs::read(stage.join("unowned")).unwrap(), b"keep");
                for index in 0..completed {
                    assert_eq!(
                        std::fs::read(stage.join(format!("payload-{index}"))).unwrap(),
                        b"staged replacement"
                    );
                }
                drop(tree);
                // The external writer removes its own file; recovery must not do so.
                std::fs::remove_file(stage.join("unowned")).unwrap();
            }
            for _ in 0..2 {
                let (tree, root) = content_root(&temporary);
                assert!(matches!(
                    root.for_pack()
                        .restore_staging_checkpoint(checkpoint.clone(), [6; 32])
                        .unwrap()
                        .reconcile(),
                    ManagedContentTransactionOutcome::Cancelled(_)
                ));
                assert!(!private.exists());
                assert!(!temporary.path().join("config").exists());
                assert_eq!(
                    std::fs::read_dir(temporary.path().join("mods"))
                        .unwrap()
                        .count(),
                    1
                );
                assert_eq!(
                    std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
                    b"original"
                );
                assert_eq!(
                    std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                    b"original manifest"
                );
                drop(tree);
            }
        }
    }

    #[test]
    fn staging_checkpoint_cadence_keeps_unrecorded_prefixes_unresolved_and_finishes_at_ready() {
        let temporary = test_tempdir().unwrap();
        let chunk = MAX_TRANSIENT_STAGE_MEMBERS;
        let total = 3 * chunk + 1;
        let (tree, mut batch) = checkpoint_batch_fixture(&temporary, total, true);
        let initial = batch.checkpoint([5; 32]).unwrap().unwrap().clone();
        let original = serde_json::to_value(&initial.record.files).unwrap();
        let mut saved = initial;
        let mut emitted = vec![0];
        for completed in 1..=total {
            batch = advance_checkpoint_payload(batch);
            if let Some(checkpoint) = batch.checkpoint([5; 32]).unwrap() {
                emitted.push(completed);
                assert_eq!(
                    serde_json::to_value(&checkpoint.record.files).unwrap(),
                    original
                );
                saved = checkpoint.clone();
            }
            assert!(batch.checkpoint([5; 32]).unwrap().is_none());
            if completed == 3 * chunk {
                assert!(matches!(
                    inspect_staging_checkpoint(&batch.state.root, &saved, false),
                    Err(ManagedContentCheckpointError::Changed)
                ));
                assert_eq!(
                    batch.state.stage.entries_bounded(total).unwrap().len(),
                    completed
                );
            }
        }
        assert_eq!(emitted, vec![0, chunk, 2 * chunk]);
        assert!(matches!(
            batch.checkpoint([4; 32]),
            Err(ManagedContentCheckpointError::Invalid)
        ));
        let ManagedContentTransferStep::Complete(complete) = batch.next() else {
            panic!("all checkpoint payloads must complete");
        };
        let ManagedContentStageOutcome::Ready(mut ready) = complete.stage() else {
            panic!("all checkpoint payloads must become Ready");
        };
        let checkpoint = ready.checkpoint([5; 32]).unwrap().clone();
        assert_eq!(checkpoint.record.payloads.len(), total);
        assert_eq!(
            serde_json::to_value(&checkpoint.record.files).unwrap(),
            original
        );
        let encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&encoded).unwrap()["schema"],
            1
        );
        drop((ready, tree));
        let (tree, root) = content_root(&temporary);
        assert!(matches!(
            root.for_pack()
                .restore_staging_checkpoint(
                    ManagedContentStagingCheckpoint::decode(&encoded).unwrap(),
                    [5; 32]
                )
                .unwrap()
                .reconcile(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
            b"original"
        );
        drop(tree);
    }

    #[test]
    fn staging_checkpoint_cached_proofs_never_refresh_after_equal_byte_drift() {
        for public in [true, false] {
            let temporary = test_tempdir().unwrap();
            let (tree, mut batch) = checkpoint_batch_fixture(&temporary, 3, true);
            batch.checkpoint([3; 32]).unwrap().unwrap();
            for _ in 0..3 {
                batch = advance_checkpoint_payload(batch);
                assert!(batch.checkpoint([3; 32]).unwrap().is_none());
            }
            let path = if public {
                temporary.path().join("mods/first.jar")
            } else {
                batch.state.stage.inner.path.join("payload-0")
            };
            let bytes = std::fs::read(&path).unwrap();
            let changed_time = std::fs::metadata(&path).unwrap().modified().unwrap()
                + std::time::Duration::from_secs(86_400);
            std::fs::write(&path, &bytes).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(changed_time))
                .unwrap();
            let ManagedContentTransferStep::Complete(complete) = batch.next() else {
                panic!("drift fixture transfers must complete");
            };
            let ManagedContentStageOutcome::Ready(mut ready) = complete.stage() else {
                panic!("drift fixture must become Ready");
            };
            for _ in 0..2 {
                assert!(matches!(
                    ready.checkpoint([3; 32]),
                    Err(ManagedContentCheckpointError::Changed)
                ));
            }
            assert_eq!(std::fs::read(path).unwrap(), bytes);
            assert!(!temporary.path().join("mods/second.jar").exists());
            drop((ready, tree));
        }
    }

    #[test]
    fn staging_checkpoint_reopens_and_repeats_exact_staged_rollback() {
        for partial_cleanup in [false, true] {
            let temporary = test_tempdir().unwrap();
            let (tree, mut ready) = checkpoint_fixture(&temporary, true);
            let checkpoint = ready.checkpoint([7; 32]).unwrap();
            let encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
            assert_eq!(
                checkpoint.encode(1),
                Err(ManagedContentCheckpointError::Capacity)
            );
            let private = ready.state.private.inner.path.clone();
            if partial_cleanup {
                let first = &ready.state.payloads[0];
                ready
                    .state
                    .stage
                    .remove_guarded_file(first.name.as_str(), first.guard.as_ref().unwrap())
                    .unwrap();
            }
            drop((ready, tree));
            for _ in 0..2 {
                let (tree, root) = content_root(&temporary);
                let checkpoint = ManagedContentStagingCheckpoint::decode(&encoded).unwrap();
                let outcome = root
                    .for_pack()
                    .restore_staging_checkpoint(checkpoint, [7; 32])
                    .unwrap()
                    .reconcile();
                assert!(matches!(
                    outcome,
                    ManagedContentTransactionOutcome::Cancelled(_)
                ));
                assert_eq!(
                    std::fs::read(temporary.path().join("mods/first.jar")).unwrap(),
                    b"original"
                );
                assert_eq!(
                    std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                    b"original manifest"
                );
                assert!(!temporary.path().join("mods/second.jar").exists());
                assert!(!temporary.path().join("config").exists());
                assert!(!private.exists());
                drop(tree);
            }
        }
    }

    #[test]
    fn staging_checkpoint_preserves_unknown_replaced_and_partly_published_objects() {
        for change in [
            "stage_extra",
            "stage_extra_after_admission",
            "backup_extra",
            "private_extra",
            "stage_replaced",
            "private_replaced",
            "ancestor_replaced",
            "same_bytes",
            "stage_same_bytes",
            "new_parent",
            "claim",
            "install",
        ] {
            let temporary = test_tempdir().unwrap();
            let (tree, mut ready) = checkpoint_fixture(&temporary, true);
            let checkpoint = ready.checkpoint([8; 32]).unwrap().clone();
            let private = ready.state.private.inner.path.clone();
            let stage = private.join(PRIVATE_STAGE_NAME);
            match change {
                "stage_extra" => std::fs::write(stage.join("unowned"), b"keep").unwrap(),
                "stage_extra_after_admission" => {}
                "backup_extra" => {
                    std::fs::write(private.join(PRIVATE_BACKUP_NAME).join("unowned"), b"keep")
                        .unwrap()
                }
                "private_extra" => {
                    std::fs::create_dir(private.join("unowned")).unwrap();
                    std::fs::write(private.join("unowned/record"), b"keep").unwrap();
                }
                "stage_replaced" => {
                    std::fs::rename(&stage, temporary.path().join("displaced-stage")).unwrap();
                    std::fs::create_dir(&stage).unwrap();
                }
                "private_replaced" => {
                    std::fs::rename(&private, temporary.path().join("displaced-private")).unwrap();
                    std::fs::create_dir(&private).unwrap();
                }
                "ancestor_replaced" => {
                    std::fs::rename(
                        temporary.path().join("mods"),
                        temporary.path().join("displaced-mods"),
                    )
                    .unwrap();
                    std::fs::create_dir(temporary.path().join("mods")).unwrap();
                    std::fs::write(temporary.path().join("mods/first.jar"), b"original").unwrap();
                }
                "same_bytes" | "stage_same_bytes" => {
                    let (path, bytes) = if change == "same_bytes" {
                        (
                            temporary.path().join("mods/first.jar"),
                            b"original".as_slice(),
                        )
                    } else {
                        (stage.join("payload-0"), b"staged replacement".as_slice())
                    };
                    let changed_time = std::fs::metadata(&path).unwrap().modified().unwrap()
                        + std::time::Duration::from_secs(86_400);
                    std::fs::write(&path, bytes).unwrap();
                    std::fs::File::options()
                        .write(true)
                        .open(&path)
                        .unwrap()
                        .set_times(std::fs::FileTimes::new().set_modified(changed_time))
                        .unwrap();
                }
                "new_parent" => std::fs::create_dir(temporary.path().join("config")).unwrap(),
                "claim" => {
                    let mutation = &mut ready.state.mutations[0];
                    mutation
                        .parent
                        .resolved()
                        .rename_guarded_file_no_replace(
                            mutation.name.as_str(),
                            mutation.old_guard.as_mut().unwrap(),
                            &ready.state.backup,
                            mutation.backup_name.as_str(),
                        )
                        .unwrap();
                }
                "install" => {
                    let mutation = &ready.state.mutations[1];
                    let payload = &mut ready.state.payloads[1];
                    ready
                        .state
                        .stage
                        .rename_guarded_file_no_replace(
                            payload.name.as_str(),
                            payload.guard.as_mut().unwrap(),
                            mutation.parent.resolved(),
                            mutation.name.as_str(),
                        )
                        .unwrap();
                }
                _ => unreachable!(),
            }
            drop((ready, tree));
            let before = std::fs::read_dir(&private).unwrap().count();
            let (tree, root) = content_root(&temporary);
            let restored = root
                .for_pack()
                .restore_staging_checkpoint(checkpoint, [8; 32]);
            let retained = if change == "stage_extra_after_admission" {
                let recovery = restored.unwrap();
                std::fs::write(stage.join("unowned"), b"keep").unwrap();
                let outcome = recovery.reconcile();
                assert!(matches!(
                    outcome,
                    ManagedContentTransactionOutcome::RecoveryRequired(_)
                ));
                Some(outcome)
            } else {
                assert!(
                    matches!(restored, Err(ManagedContentCheckpointError::Changed)),
                    "{change}"
                );
                None
            };
            assert_eq!(
                std::fs::read_dir(&private).unwrap().count(),
                before,
                "{change}"
            );
            match change {
                "stage_extra" | "stage_extra_after_admission" => {
                    assert_eq!(std::fs::read(stage.join("unowned")).unwrap(), b"keep")
                }
                "backup_extra" => assert_eq!(
                    std::fs::read(private.join("backup/unowned")).unwrap(),
                    b"keep"
                ),
                "private_extra" => assert_eq!(
                    std::fs::read(private.join("unowned/record")).unwrap(),
                    b"keep"
                ),
                "stage_replaced" => assert!(stage.is_dir()),
                "private_replaced" => assert!(private.is_dir()),
                "ancestor_replaced" => assert_eq!(
                    std::fs::read(temporary.path().join("displaced-mods/first.jar")).unwrap(),
                    b"original"
                ),
                "new_parent" => assert!(temporary.path().join("config").is_dir()),
                "claim" => assert!(!temporary.path().join("mods/first.jar").exists()),
                _ => {}
            }
            let original = if change == "claim" {
                private.join("backup/old-0")
            } else {
                temporary.path().join("mods/first.jar")
            };
            assert_eq!(std::fs::read(original).unwrap(), b"original", "{change}");
            assert_eq!(
                std::fs::read(temporary.path().join(MANIFEST_NAME)).unwrap(),
                b"original manifest",
                "{change}"
            );
            let retained_stage = match change {
                "stage_replaced" => temporary.path().join("displaced-stage"),
                "private_replaced" => temporary.path().join("displaced-private/stage"),
                _ => stage,
            };
            for index in 0..3 {
                let path = if change == "install" && index == 1 {
                    temporary.path().join("mods/second.jar")
                } else {
                    retained_stage.join(format!("payload-{index}"))
                };
                assert_eq!(
                    std::fs::read(path).unwrap(),
                    b"staged replacement",
                    "{change}"
                );
            }
            drop((retained, tree));
        }
    }

    #[test]
    fn staging_checkpoint_rejects_wrong_authority_codec_and_capture_drift() {
        let temporary = test_tempdir().unwrap();
        let (tree, mut ready) = checkpoint_fixture(&temporary, true);
        let checkpoint = ready.checkpoint([9; 32]).unwrap().clone();
        let encoded = checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap();
        let duplicate_parent =
            encoded.replacen("\"directories\":{", "\"directories\":{\"mods\":null,", 1);
        assert!(matches!(
            ManagedContentStagingCheckpoint::decode(&duplicate_parent),
            Err(ManagedContentCheckpointError::Invalid)
        ));
        let mut record: serde_json::Value =
            serde_json::from_str(&checkpoint.encode(MAX_STAGING_CHECKPOINT_BYTES).unwrap())
                .unwrap();
        record["payloads"][0]["name"] = "../unowned".into();
        assert!(matches!(
            ManagedContentStagingCheckpoint::decode(&record.to_string()),
            Err(ManagedContentCheckpointError::Invalid)
        ));
        let path = temporary.path().join("mods/first.jar");
        let changed_time = std::fs::metadata(&path).unwrap().modified().unwrap()
            + std::time::Duration::from_secs(86_400);
        std::fs::write(&path, b"original").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(changed_time))
            .unwrap();
        assert!(matches!(
            ready.checkpoint([9; 32]),
            Err(ManagedContentCheckpointError::Changed)
        ));
        drop((ready, tree));
        let (tree, root) = content_root(&temporary);
        assert!(matches!(
            root.for_pack()
                .restore_staging_checkpoint(checkpoint.clone(), [10; 32]),
            Err(ManagedContentCheckpointError::Invalid)
        ));
        drop(tree);
        let other = test_tempdir().unwrap();
        let (tree, root) = content_root(&other);
        assert!(matches!(
            root.for_pack()
                .restore_staging_checkpoint(checkpoint, [9; 32]),
            Err(ManagedContentCheckpointError::Changed)
        ));
        drop(tree);
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
                path_policy: ManagedContentPathPolicy::Managed,
            },
        );
        assert!(plan.is_ok());

        let reserved = PortableRelativePath::new_exact("mods/axial.content.json")
            .expect("portable reserved path");
        assert_eq!(
            validate_content_path(ManagedContentPathPolicy::Managed, &reserved),
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
            let path =
                PortableRelativePath::new_exact(&format!("mods/budget-{index}.jar")).expect("path");
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
                    path_policy: ManagedContentPathPolicy::Managed,
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
                    path_policy: ManagedContentPathPolicy::Managed,
                },
            ),
            Err(ManagedContentPlanError::TransactionBudgetExceeded)
        ));
    }

    #[test]
    fn manifest_first_planning_is_incremental_and_selects_one_inspected_subset() {
        let temporary = test_tempdir().expect("temporary instance");
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
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::DuplicatePath
        );
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
    fn deferred_manifest_refusal_retains_complete_transaction_for_exact_binding() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/deferred.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = deferred_absent_plan(&session, path);
        let complete = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Complete(complete) => complete,
            ManagedContentTransferStep::Issued(_) => panic!("empty plan must be complete"),
        };
        assert_eq!(complete.reports().len(), 0);
        let complete = match complete.bind_manifest(Vec::new()) {
            ManagedContentManifestBindOutcome::Refused {
                error: ManagedContentPlanError::InvalidManifest,
                transfers,
            } => transfers,
            _ => panic!("empty deferred manifest must retain the complete owner"),
        };
        let complete = match complete.bind_manifest(b"late-manifest".to_vec()) {
            ManagedContentManifestBindOutcome::Bound(complete) => complete,
            _ => panic!("bounded deferred manifest must bind"),
        };
        let ready = match complete.stage() {
            ManagedContentStageOutcome::Ready(ready) => ready,
            ManagedContentStageOutcome::Unwind(_) => panic!("bound transaction must stage"),
        };
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Committed(_)
        ));
        assert_eq!(
            std::fs::read(temporary.path().join(MANIFEST_NAME)).expect("committed manifest"),
            b"late-manifest"
        );
    }

    #[test]
    fn external_reader_and_late_manifest_share_one_transaction_owner() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let root = root.for_pack();
        let path = PortableRelativePath::new_exact("config/nested/external.toml").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        assert!(!temporary.path().join("config").exists());
        let source = b"authenticated external bytes";
        let id = ManagedContentPayloadId::new("external").expect("payload id");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(source.len() as u64).expect("nonempty source"),
            crate::download::ExpectedTransferDigests::sha512(<[u8; 64]>::from(Sha512::digest(
                source,
            ))),
        )
        .expect("external contract");
        let plan = ManagedContentMutationPlan::new_deferred(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                path.clone(),
                ManagedContentObservedState::Absent,
                ManagedContentPathResult::Download(id.clone()),
            )],
            vec![ManagedContentPayloadPlan::from_external_source(
                id.clone(),
                contract,
            )],
            session.defer_manifest(),
        )
        .expect("external plan");
        let issued = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Issued(issued) => issued,
            ManagedContentTransferStep::Complete(_) => panic!("external payload must be issued"),
        };
        assert!(issued.is_external());
        assert!(!issued.is_local());
        let (_cancellation, cancelled) = crate::download::transfer_cancellation_channel();
        let settlement = issued
            .copy_external(std::io::Cursor::new(source), cancelled)
            .expect("external slot accepts its reader");
        let batch = match settlement.advance() {
            ManagedContentTransferAdvance::Continue(batch) => batch,
            ManagedContentTransferAdvance::Unwind(_) => panic!("external copy must verify"),
        };
        let complete = match batch.next() {
            ManagedContentTransferStep::Complete(complete) => complete,
            ManagedContentTransferStep::Issued(_) => panic!("external batch must be complete"),
        };
        let reports = complete.reports().collect::<Vec<_>>();
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].0, &id);
        assert_eq!(reports[0].1.bytes(), source.len() as u64);
        let complete = match complete.bind_manifest(b"external-manifest".to_vec()) {
            ManagedContentManifestBindOutcome::Bound(complete) => complete,
            _ => panic!("late external manifest must bind"),
        };
        let ready = match complete.stage() {
            ManagedContentStageOutcome::Ready(ready) => ready,
            ManagedContentStageOutcome::Unwind(_) => panic!("external payload must stage"),
        };
        assert!(!temporary.path().join("config").exists());
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Committed(_)
        ));
        assert_eq!(
            std::fs::read(path.join_under(temporary.path())).expect("published external payload"),
            source
        );
    }

    #[test]
    fn pack_transfers_publish_private_stages_across_effect_windows() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        // Preserve the original 513-member regression on every platform. This
        // crosses two Linux windows and three portable named-stage windows.
        const PAYLOAD_COUNT: usize = 513;
        assert!(PAYLOAD_COUNT > MAX_TRANSIENT_STAGE_MEMBERS);
        let paths = (0..PAYLOAD_COUNT)
            .map(|index| {
                PortableRelativePath::new_exact(&format!("config/bulk/file-{index}.toml"))
                    .expect("pack path")
            })
            .collect::<Vec<_>>();
        let session = transaction_session(root.for_pack(), paths.clone());
        let observations = session.observations();
        let source = b"x";
        let digest = <[u8; 64]>::from(Sha512::digest(source));
        let mut mutations = Vec::with_capacity(paths.len());
        let mut payloads = Vec::with_capacity(paths.len());
        for (index, (path, observation)) in paths.into_iter().zip(&observations).enumerate() {
            let id =
                ManagedContentPayloadId::new(&format!("pack-{index}")).expect("pack payload id");
            let contract = TransferContract::authenticated_exact(
                std::num::NonZeroU64::new(1).expect("one is nonzero"),
                crate::download::ExpectedTransferDigests::sha512(digest),
            )
            .expect("pack contract");
            mutations.push(ManagedContentPathMutation::new(
                path,
                observation.state().clone(),
                ManagedContentPathResult::Download(id.clone()),
            ));
            payloads.push(ManagedContentPayloadPlan::from_external_source(
                id, contract,
            ));
        }
        let plan = ManagedContentMutationPlan::new_deferred(
            &observations,
            mutations,
            payloads,
            session.defer_manifest(),
        )
        .expect("large pack plan");
        let mut transfers = prepared(session, plan).into_transfer_batch();
        let complete = loop {
            match transfers.next() {
                ManagedContentTransferStep::Issued(issued) => {
                    let (_cancellation, cancelled) =
                        crate::download::transfer_cancellation_channel();
                    let settlement = issued
                        .copy_external(std::io::Cursor::new(source), cancelled)
                        .expect("pack slot accepts external bytes");
                    transfers = match settlement.advance() {
                        ManagedContentTransferAdvance::Continue(next) => next,
                        ManagedContentTransferAdvance::Unwind(_) => {
                            panic!("pack transfer window must publish")
                        }
                    };
                }
                ManagedContentTransferStep::Complete(complete) => break complete,
            }
        };
        assert_eq!(complete.reports().len(), PAYLOAD_COUNT);
        assert!(!temporary.path().join("config").exists());
        assert!(matches!(
            complete.cancel(),
            ManagedContentTransactionOutcome::Cancelled(_)
        ));
        assert!(!temporary.path().join("config").exists());
    }

    #[test]
    fn pack_planning_preserves_indexed_bytes_above_the_managed_budget() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let paths = (0..5)
            .map(|index| {
                PortableRelativePath::new_exact(&format!("config/large-{index}.bin"))
                    .expect("pack path")
            })
            .collect::<Vec<_>>();
        let session = transaction_session(root.for_pack(), paths.clone());
        let observations = session.observations();
        let mut mutations = Vec::with_capacity(paths.len());
        let mut payloads = Vec::with_capacity(paths.len());
        for (index, (path, observation)) in paths.into_iter().zip(&observations).enumerate() {
            let id =
                ManagedContentPayloadId::new(&format!("large-{index}")).expect("pack payload id");
            let contract = TransferContract::authenticated_exact(
                std::num::NonZeroU64::new(MAX_CONTENT_FILE_BYTES).expect("limit is nonzero"),
                crate::download::ExpectedTransferDigests::sha512([index as u8; 64]),
            )
            .expect("pack contract");
            mutations.push(ManagedContentPathMutation::new(
                path,
                observation.state().clone(),
                ManagedContentPathResult::Download(id.clone()),
            ));
            payloads.push(ManagedContentPayloadPlan::from_external_source(
                id, contract,
            ));
        }
        assert!(
            ManagedContentMutationPlan::new_deferred(
                &observations,
                mutations,
                payloads,
                session.defer_manifest(),
            )
            .is_ok(),
            "the legacy pack path had no four-GiB aggregate indexed-download ceiling"
        );
    }

    #[test]
    fn pack_rollback_removes_the_exact_created_parent_chain() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect =
            PortableRelativePath::new_exact("config/nested/rollback.toml").expect("effect path");
        let dependency =
            PortableRelativePath::new_exact("mods/dependency.jar").expect("dependency path");
        let dependency_path = dependency.join_under(temporary.path());
        std::fs::write(&dependency_path, b"dependency").expect("dependency");
        let session = transaction_session_with_effects(
            root.for_pack(),
            vec![effect.clone(), dependency],
            vec![effect.clone()],
        );
        let source = b"rollback bytes";
        let id = ManagedContentPayloadId::new("rollback").expect("payload id");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(source.len() as u64).expect("nonempty source"),
            crate::download::ExpectedTransferDigests::sha512(<[u8; 64]>::from(Sha512::digest(
                source,
            ))),
        )
        .expect("external contract");
        let manifest = session
            .bind_encoded_manifest(b"rollback-manifest".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &session.observations(),
            vec![ManagedContentPathMutation::new(
                effect.clone(),
                ManagedContentObservedState::Absent,
                ManagedContentPathResult::Download(id.clone()),
            )],
            vec![ManagedContentPayloadPlan::from_external_source(
                id, contract,
            )],
            manifest,
        )
        .expect("rollback plan");
        let issued = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Issued(issued) => issued,
            ManagedContentTransferStep::Complete(_) => panic!("external payload must be issued"),
        };
        let (_cancellation, cancelled) = crate::download::transfer_cancellation_channel();
        let batch = match issued
            .copy_external(std::io::Cursor::new(source), cancelled)
            .expect("external copy")
            .advance()
        {
            ManagedContentTransferAdvance::Continue(batch) => batch,
            ManagedContentTransferAdvance::Unwind(_) => panic!("external copy must verify"),
        };
        let complete = match batch.next() {
            ManagedContentTransferStep::Complete(complete) => complete,
            ManagedContentTransferStep::Issued(_) => panic!("batch must be complete"),
        };
        let mut ready = match complete.stage() {
            ManagedContentStageOutcome::Ready(ready) => ready,
            ManagedContentStageOutcome::Unwind(_) => panic!("payload must stage"),
        };
        ready.state.before_manifest_revalidation = Some(Box::new(move || {
            std::fs::write(dependency_path, b"drifted").expect("drift dependency");
        }));
        assert!(matches!(
            ready.commit(),
            ManagedContentTransactionOutcome::Failed(
                ManagedContentTransactionFailure::ObservationDrift
            )
        ));
        assert!(!effect.join_under(temporary.path()).exists());
        assert!(!temporary.path().join("config").exists());
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
    }

    #[test]
    fn unbound_deferred_manifest_unwinds_without_namespace_effects() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let root = root.for_pack();
        let path = PortableRelativePath::new_exact("config/nested/unbound.toml").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = deferred_absent_plan(&session, path);
        let complete = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Complete(complete) => complete,
            ManagedContentTransferStep::Issued(_) => panic!("empty plan must be complete"),
        };
        assert!(matches!(
            complete.stage(),
            ManagedContentStageOutcome::Unwind(ManagedContentTransactionOutcome::Cancelled(_))
        ));
        assert!(!temporary.path().join(MANIFEST_NAME).exists());
        assert!(!temporary.path().join("config").exists());
    }

    #[test]
    fn pack_observation_rejects_portable_parent_aliases_before_effects() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let upper = PortableRelativePath::new_exact("Config/first.toml").expect("upper path");
        let lower = PortableRelativePath::new_exact("config/second.toml").expect("lower path");
        let failure = root
            .for_pack()
            .observe_manifest()
            .expect("manifest observation")
            .observe_more(vec![upper, lower])
            .expect_err("portable parent aliases must be rejected");
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::NonPortableEntry
        );
        assert!(failure.into_session().observations().is_empty());
        assert!(!temporary.path().join("Config").exists());
        assert!(!temporary.path().join("config").exists());
    }

    #[test]
    fn planning_binding_matches_only_its_exact_planning_flow() {
        let first = test_tempdir().expect("first instance");
        let second = test_tempdir().expect("second instance");
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
            let temporary = test_tempdir().expect("temporary instance");
            std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
            std::fs::write(
                temporary.path().join("mods").join(alias),
                fixture.as_bytes(),
            )
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
        let temporary = test_tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/example.jar"), b"enabled").expect("enabled");
        std::fs::write(
            temporary.path().join("mods/example.jar.disabled"),
            b"disabled",
        )
        .expect("disabled");
        let (_tree, root) = content_root(&temporary);
        let enabled = PortableRelativePath::new_exact("mods/example.jar").expect("enabled path");
        let disabled =
            PortableRelativePath::new_exact("mods/example.jar.disabled").expect("disabled path");
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
        let temporary = test_tempdir().expect("temporary instance");
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
        let planning = root
            .observe_manifest()
            .expect("absent manifest observation");
        assert_eq!(
            planning.manifest_state(),
            &ManagedContentObservedState::Absent
        );
        assert_eq!(planning.manifest_bytes(), None);
    }

    #[test]
    fn plan_from_an_aliased_session_cannot_bind_a_later_exact_guard() {
        let temporary = test_tempdir().expect("temporary instance");
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
        let temporary = test_tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/first.jar"), b"first").expect("first");
        let (_tree, root) = content_root(&temporary);
        std::fs::remove_dir(temporary.path().join("resourcepacks"))
            .expect("remove unavailable parent");
        let first = PortableRelativePath::new_exact("mods/first.jar").expect("first path");
        let unavailable =
            PortableRelativePath::new_exact("resourcepacks/later.zip").expect("unavailable path");

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
        assert_eq!(
            failure.error(),
            ManagedContentObservationError::DuplicatePath
        );
    }

    #[test]
    fn prepared_cancel_removes_its_reserved_namespace() {
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let path = PortableRelativePath::new_exact("mods/cancelled.jar").expect("path");
        let session = transaction_session(root, vec![path.clone()]);
        let plan = absent_plan(&session, path);
        let outcome = prepared(session, plan).cancel();
        assert!(matches!(
            outcome,
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
    fn transfer_batch_issues_only_the_next_exact_slot() {
        let temporary = test_tempdir().expect("temporary instance");
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
    fn local_transfer_rejects_source_drift_and_unwinds_without_publication() {
        let temporary = test_tempdir().expect("temporary instance");
        let source = PortableRelativePath::new_exact("mods/source.jar").expect("source path");
        let target =
            PortableRelativePath::new_exact("mods/source.jar.disabled").expect("target path");
        let source_path = source.join_under(temporary.path());
        let target_path = target.join_under(temporary.path());
        let original = b"source bytes";
        let (_tree, root) = content_root(&temporary);
        std::fs::write(&source_path, original).expect("source bytes");
        let session = transaction_session(root, vec![source.clone(), target.clone()]);
        let observations = session.observations();
        let id = ManagedContentPayloadId::new("local-copy").expect("payload id");
        let contract = TransferContract::authenticated_exact(
            std::num::NonZeroU64::new(original.len() as u64).expect("nonzero source"),
            crate::download::ExpectedTransferDigests::sha512(<[u8; 64]>::from(Sha512::digest(
                original,
            ))),
        )
        .expect("local transfer contract");
        let manifest = session
            .bind_encoded_manifest(b"{}".to_vec())
            .expect("manifest");
        let plan = ManagedContentMutationPlan::new(
            &observations,
            vec![
                ManagedContentPathMutation::new(
                    source.clone(),
                    observations[0].state().clone(),
                    ManagedContentPathResult::Absent,
                ),
                ManagedContentPathMutation::new(
                    target,
                    observations[1].state().clone(),
                    ManagedContentPathResult::Download(id.clone()),
                ),
            ],
            vec![ManagedContentPayloadPlan::from_observation(
                id, contract, source,
            )],
            manifest,
        )
        .expect("local transfer plan");
        let issued = match prepared(session, plan).into_transfer_batch().next() {
            ManagedContentTransferStep::Issued(issued) => issued,
            ManagedContentTransferStep::Complete(_) => panic!("local transfer must be issued"),
        };
        std::fs::write(&source_path, b"drifted source bytes").expect("drift source");
        let (_cancellation, cancelled) = crate::download::transfer_cancellation_channel();
        let settlement = issued.copy_local(cancelled);
        assert!(matches!(
            settlement
                .failure_report()
                .expect("source drift failure")
                .last(),
            crate::download::TransferFailureKind::SourceRead(_)
        ));
        assert!(matches!(
            settlement.advance(),
            ManagedContentTransferAdvance::Unwind(ManagedContentTransactionOutcome::Cancelled(_))
        ));
        assert_eq!(
            std::fs::read(source_path).expect("drifted source remains"),
            b"drifted source bytes"
        );
        assert!(!target_path.exists());
    }

    #[test]
    fn complete_unstarted_batch_drives_transaction_cancellation() {
        let temporary = test_tempdir().expect("temporary instance");
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
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
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
            source: _,
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

        assert!(matches!(
            settlement.advance(),
            ManagedContentTransferAdvance::Unwind(ManagedContentTransactionOutcome::Cancelled(_))
        ));
    }

    #[test]
    fn uninstall_commit_removes_observed_file_and_publishes_manifest() {
        let temporary = test_tempdir().expect("temporary instance");
        std::fs::create_dir_all(temporary.path().join("mods")).expect("mods");
        std::fs::write(temporary.path().join("mods/remove.jar"), b"old").expect("old content");
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
        let temporary = test_tempdir().expect("temporary instance");
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
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/effect.jar").expect("effect path");
        let mut observed_paths = vec![effect.clone()];
        observed_paths.extend((0..=MAX_CONTENT_PATHS).map(|index| {
            PortableRelativePath::new_exact(&format!("mods/precondition-{index}.jar"))
                .expect("precondition path")
        }));
        let session = transaction_session_with_effects(root, observed_paths, vec![effect.clone()]);
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
        let temporary = test_tempdir().expect("temporary instance");
        let (_tree, root) = content_root(&temporary);
        let effect = PortableRelativePath::new_exact("mods/remove.jar").expect("effect path");
        let dependency =
            PortableRelativePath::new_exact("mods/dependency.jar").expect("dependency path");
        std::fs::write(effect.join_under(temporary.path()), b"old").expect("old effect");
        std::fs::write(dependency.join_under(temporary.path()), b"dependency").expect("dependency");
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
        let temporary = test_tempdir().expect("temporary instance");
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
        let temporary = test_tempdir().expect("temporary instance");
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
            ManagedContentTransactionOutcome::Failed(ManagedContentTransactionFailure::ClaimFailed)
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
        let temporary = test_tempdir().expect("temporary instance");
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
