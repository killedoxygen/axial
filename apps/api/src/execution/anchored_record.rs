//! Identity-bound access to exact regular files below held no-follow directories.

use std::ffi::{OsStr, OsString};
use std::collections::HashMap;
use std::io;
use std::io::Read as _;
use std::sync::{Arc, Mutex, Weak};

#[cfg(test)]
use std::path::{Path, PathBuf};

use axial_config::AppRootSession;
use axial_fs::{
    Directory, DirectoryIdentity, DirectoryListingState, DirectoryRevision, EffectOwner,
    ExpectedFileContent, FileCapability, FileCreateOutcome, FileCreateResolution,
    FileParkObligation, FileParkOutcome, FileParkPreservationError, FileParkResolution,
    FileParkRequestSource,
    FileRemovalOutcome, FileReplaceOutcome, FileReplaceReceipt, FileReplaceReceiptOutcome,
    FileRevision, LeafName, LeafNameEquivalenceKey, ParkedFile, ReplaceDestination,
    SealedStagedFile, StageDiscardOutcome, leaf_name_equivalence_keys, leaf_names_equivalent,
};
use sha2::{Digest as _, Sha256};
use sha2::Sha512;

const RESTART_IDENTITY_DOMAIN: &[u8] = b"axial.persisted-state-restart-record-identity.v3\0";
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
const ALIAS_VALIDATION_ATTEMPTS: usize = 3;

pub(crate) struct AnchoredRecordIdentity {
    directory: AnchoredRecordDirectory,
    file: FileCapability,
    leaf: LeafName,
    revision: FileRevision,
    quarantine_sha256: Option<[u8; 32]>,
}

#[must_use = "parked-file receipt must be acknowledged or retained"]
pub(crate) struct AnchoredRecordQuarantineReceipt {
    parked: ParkedFile,
    directory: AnchoredRecordDirectory,
    original: LeafName,
    parked_leaf: LeafName,
}

#[must_use = "unsettled quarantine preservation retains parked-file authority"]
pub(crate) enum AnchoredRecordQuarantinePreservationError {
    Acknowledgement {
        error: FileParkPreservationError,
        _directory: AnchoredRecordDirectory,
    },
    Alias {
        error: io::Error,
        _receipt: AnchoredRecordQuarantineReceipt,
    },
    IndeterminatePark {
        obligation: FileParkObligation,
        _root_session: Arc<AppRootSession>,
    },
}

pub(crate) enum AnchoredRecordQuarantineError {
    Refused(io::Error),
    AppliedUnverified {
        obligation: FileParkObligation,
        _root_session: Arc<AppRootSession>,
    },
}

impl std::fmt::Debug for AnchoredRecordQuarantineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnchoredRecordQuarantineError")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for AnchoredRecordQuarantineError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Refused(_) => formatter.write_str("anchored record quarantine was refused"),
            Self::AppliedUnverified { .. } => {
                formatter.write_str("anchored record quarantine could not be verified")
            }
        }
    }
}

impl std::error::Error for AnchoredRecordQuarantineError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Refused(error) => Some(error),
            Self::AppliedUnverified { obligation, .. } => Some(obligation.error()),
        }
    }
}

pub(crate) struct AnchoredRecordRestartDigest([u8; 32]);

pub(crate) struct AnchoredRecordRetirement {
    target: AnchoredRecordTarget,
    effects: EffectOwner,
}

pub(crate) struct AnchoredRecordRetirementFailure {
    error: io::Error,
    retirement: Option<AnchoredRecordRetirement>,
}

#[derive(Default)]
pub(crate) struct AnchoredRecordRetirementSlot {
    pending: Mutex<Option<AnchoredRecordRetirement>>,
}

impl AnchoredRecordRetirementSlot {
    pub(crate) fn retain_failure(&self, failure: AnchoredRecordRetirementFailure) -> io::Error {
        let (error, retirement) = failure.into_parts();
        if let Some(retirement) = retirement {
            let mut pending = self
                .pending
                .lock()
                .expect("anchored record retirement slot lock poisoned");
            debug_assert!(pending.is_none());
            if pending.is_none() {
                *pending = Some(retirement);
            }
        }
        error
    }

    fn retry_blocking(&self) -> io::Result<()> {
        let retirement = self
            .pending
            .lock()
            .expect("anchored record retirement slot lock poisoned")
            .take();
        let Some(retirement) = retirement else {
            return Ok(());
        };
        match retirement.retry() {
            Ok(()) => Ok(()),
            Err(failure) => Err(self.retain_failure(failure)),
        }
    }

    pub(crate) async fn retry(self: &Arc<Self>) -> io::Result<()> {
        let retirement = self.clone();
        tokio::task::spawn_blocking(move || retirement.retry_blocking())
            .await
            .map_err(|error| {
                io::Error::other(format!("anchored record retirement task failed: {error}"))
            })?
    }
}

impl AnchoredRecordRetirementFailure {
    pub(crate) fn into_parts(self) -> (io::Error, Option<AnchoredRecordRetirement>) {
        (self.error, self.retirement)
    }
}

impl std::fmt::Debug for AnchoredRecordRetirementFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnchoredRecordRetirementFailure")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for AnchoredRecordRetirementFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("anchored record retirement remains unsettled")
    }
}

impl std::error::Error for AnchoredRecordRetirementFailure {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.error)
    }
}

impl AnchoredRecordRetirement {
    pub(crate) fn retry(self) -> Result<(), AnchoredRecordRetirementFailure> {
        let result = self
            .target
            .settle(&self.effects)
            .and_then(|()| self.target.remove(&self.effects))
            .and_then(|()| self.target.settle(&self.effects))
            .and_then(|()| settle_effects_complete(&self.effects));
        match result {
            Ok(()) => Ok(()),
            Err(error) => Err(AnchoredRecordRetirementFailure {
                error,
                retirement: Some(self),
            }),
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) enum AnchoredRecordRestartContext {
    PerformanceOperation,
    BenchmarkSuiteDriver,
}

#[derive(Clone)]
pub(crate) struct AnchoredRecordDirectory {
    directory: Directory,
    root_session: Arc<AppRootSession>,
    records: Arc<Mutex<AnchoredRecordRegistry>>,
    alias_inventory: Arc<Mutex<Option<AnchoredRecordAliasInventory>>>,
    #[cfg(test)]
    test_path: Option<PathBuf>,
}

struct AnchoredRecordRegistration {
    leaf: LeafName,
    keys: Vec<LeafNameEquivalenceKey>,
    mutation: Weak<Mutex<AnchoredRecordMutationState>>,
    admitted: Option<Arc<Mutex<AnchoredRecordMutationState>>>,
}

#[derive(Default)]
struct AnchoredRecordRegistry {
    next_id: u64,
    records: HashMap<u64, AnchoredRecordRegistration>,
    index: HashMap<LeafNameEquivalenceKey, Vec<u64>>,
    #[cfg(test)]
    peak_admitted: usize,
}

struct AnchoredRecordAliasInventory {
    revision: DirectoryRevision,
    names: HashMap<LeafNameEquivalenceKey, Vec<OsString>>,
}

impl AnchoredRecordRegistry {
    fn lookup(
        &mut self,
        leaf: &LeafName,
    ) -> io::Result<Option<Arc<Mutex<AnchoredRecordMutationState>>>> {
        let keys = leaf_name_equivalence_keys(leaf.as_os_str());
        let candidate_ids = keys
            .iter()
            .filter_map(|key| self.index.get(key))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        let mut expired = Vec::new();
        for id in candidate_ids {
            let Some(record) = self.records.get(&id) else {
                continue;
            };
            let mutation = record
                .admitted
                .clone()
                .or_else(|| record.mutation.upgrade());
            let Some(mutation) = mutation else {
                expired.push(id);
                continue;
            };
            if !leaf_names_equivalent(record.leaf.as_os_str(), leaf.as_os_str()) {
                continue;
            }
            if record.leaf != *leaf {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "portable-equivalent anchored record name is already registered",
                ));
            }
            return Ok(Some(mutation));
        }
        expired.sort_unstable();
        expired.dedup();
        for id in expired {
            self.remove(id);
        }
        Ok(None)
    }

    fn insert(
        &mut self,
        leaf: LeafName,
        mutation: &Arc<Mutex<AnchoredRecordMutationState>>,
    ) -> io::Result<()> {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).ok_or_else(|| {
            io::Error::other("anchored record registration identity space exhausted")
        })?;
        let keys = leaf_name_equivalence_keys(leaf.as_os_str());
        for key in &keys {
            self.index.entry(key.clone()).or_default().push(id);
        }
        self.records.insert(
            id,
            AnchoredRecordRegistration {
                leaf,
                keys,
                mutation: Arc::downgrade(mutation),
                admitted: None,
            },
        );
        Ok(())
    }

    fn retain_admitted(
        &mut self,
        leaf: &LeafName,
        mutation: &Arc<Mutex<AnchoredRecordMutationState>>,
    ) {
        let candidate_ids = leaf_name_equivalence_keys(leaf.as_os_str())
            .iter()
            .filter_map(|key| self.index.get(key))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        for id in candidate_ids {
            let Some(record) = self.records.get_mut(&id) else {
                continue;
            };
            if record.leaf == *leaf
                && record
                    .mutation
                    .upgrade()
                    .is_some_and(|registered| Arc::ptr_eq(&registered, mutation))
            {
                record.admitted = Some(mutation.clone());
                break;
            }
        }
        #[cfg(test)]
        {
            let admitted = self
                .records
                .values()
                .filter(|record| record.admitted.is_some())
                .count();
            self.peak_admitted = self.peak_admitted.max(admitted);
        }
    }

    fn release(
        &mut self,
        leaf: &LeafName,
        mutation: &Arc<Mutex<AnchoredRecordMutationState>>,
    ) {
        let candidate_ids = leaf_name_equivalence_keys(leaf.as_os_str())
            .iter()
            .filter_map(|key| self.index.get(key))
            .flatten()
            .copied()
            .collect::<Vec<_>>();
        for id in candidate_ids {
            let matches = self.records.get(&id).is_some_and(|record| {
                record.leaf == *leaf
                    && record
                        .mutation
                        .upgrade()
                        .is_none_or(|registered| Arc::ptr_eq(&registered, mutation))
            });
            if matches {
                self.remove(id);
                return;
            }
        }
    }

    fn remove(&mut self, id: u64) {
        let Some(record) = self.records.remove(&id) else {
            return;
        };
        for key in record.keys {
            let remove_bucket = if let Some(bucket) = self.index.get_mut(&key) {
                bucket.retain(|candidate| *candidate != id);
                bucket.is_empty()
            } else {
                false
            };
            if remove_bucket {
                self.index.remove(&key);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct AnchoredRecordTarget {
    directory: AnchoredRecordDirectory,
    leaf: LeafName,
    max_existing_bytes: u64,
    mutation: Arc<Mutex<AnchoredRecordMutationState>>,
}

#[derive(Default)]
struct AnchoredRecordMutationState {
    published: Option<PublishedRecord>,
    pending_replace: Option<PendingRecordReplace>,
    delete: Option<AnchoredRecordDeleteState>,
    source_latched: bool,
    terminal: bool,
    alias_latched: bool,
}

enum AnchoredRecordDeleteState {
    Source(axial_fs::FileParkRequest),
    Park(FileParkObligation),
    Retired,
}

struct PublishedRecord {
    file: FileCapability,
    revision: FileRevision,
    sha256: [u8; 32],
    size: u64,
}

struct PendingRecordReplace {
    receipt: FileReplaceReceipt,
    sha256: [u8; 32],
    size: u64,
}

enum AnchoredRecordSource {
    Vacant,
    Current(axial_fs::FileParkRequest),
    Displaced,
}

#[derive(Eq, PartialEq)]
pub(crate) struct AnchoredRecordDirectoryEpoch(DirectoryRevision);

pub(crate) struct AnchoredRecordDigestObservation {
    sha256: [u8; 32],
    sha512: [u8; 64],
    size: u64,
    modified_at_ns: u64,
    identity: AnchoredRecordIdentity,
}

pub(crate) enum AnchoredRecordObservation {
    Bytes {
        bytes: Vec<u8>,
        identity: AnchoredRecordIdentity,
    },
    Oversized {
        identity: AnchoredRecordIdentity,
    },
}

impl AnchoredRecordObservation {
    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes { bytes, .. } => Some(bytes),
            Self::Oversized { .. } => None,
        }
    }

    pub(crate) fn is_oversized(&self) -> bool {
        matches!(self, Self::Oversized { .. })
    }

    pub(crate) fn admit(self, max_existing_bytes: u64) -> io::Result<AnchoredRecordTarget> {
        match self {
            Self::Bytes { identity, .. } => identity.admit(max_existing_bytes),
            Self::Oversized { .. } => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized anchored record cannot be admitted for mutation",
            )),
        }
    }

    pub(crate) fn retire(
        self,
        max_existing_bytes: u64,
    ) -> Result<(), AnchoredRecordRetirementFailure> {
        let effects = match &self {
            Self::Bytes { identity, .. } | Self::Oversized { identity } => {
                identity.directory.effect_owner()
            }
        }
        .map_err(|error| AnchoredRecordRetirementFailure {
            error,
            retirement: None,
        })?;
        let target = self.admit(max_existing_bytes).map_err(|error| {
            AnchoredRecordRetirementFailure {
                error,
                retirement: None,
            }
        })?;
        AnchoredRecordRetirement { target, effects }.retry()
    }

    pub(crate) fn into_restart_identity(
        self,
        context: AnchoredRecordRestartContext,
        canonical_original_name: &LeafName,
    ) -> io::Result<(AnchoredRecordIdentity, AnchoredRecordRestartDigest)> {
        let (identity, bytes) = match self {
            Self::Bytes { bytes, identity } => (identity, bytes),
            Self::Oversized { .. } => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "oversized anchored records have no restart identity",
                ));
            }
        };
        identity.revalidate()?;
        let mut hasher = Sha256::new();
        hasher.update(RESTART_IDENTITY_DOMAIN);
        let store_domain: &[u8] = match context {
            AnchoredRecordRestartContext::PerformanceOperation => b"performance-operation\0",
            AnchoredRecordRestartContext::BenchmarkSuiteDriver => b"benchmark-suite-driver\0",
        };
        hasher.update(store_domain);
        update_native_name(&mut hasher, canonical_original_name);
        hasher.update(b"regular-file\0");
        let size = identity.revision.size();
        let modified_at_ns = identity.revision.modified_at_ns()?;
        hasher.update(size.to_le_bytes());
        hasher.update(modified_at_ns.to_le_bytes());
        hasher.update(b"full\0");
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
        identity.revalidate()?;
        Ok((
            identity,
            AnchoredRecordRestartDigest(hasher.finalize().into()),
        ))
    }
}

impl AnchoredRecordRestartDigest {
    pub(crate) fn into_bytes(self) -> [u8; 32] {
        self.0
    }
}

impl AnchoredRecordDirectory {
    pub(crate) fn from_directory(
        root_session: Arc<AppRootSession>,
        directory: Directory,
    ) -> Self {
        Self {
            directory,
            root_session,
            records: Arc::new(Mutex::new(AnchoredRecordRegistry::default())),
            alias_inventory: Arc::new(Mutex::new(None)),
            #[cfg(test)]
            test_path: None,
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_directory(path: &Path) -> io::Result<Self> {
        let paths = axial_config::AppPaths::from_root(path).map_err(io::Error::other)?;
        let root_session = Arc::new(paths.open_root_session()?);
        let directory = root_session.root_directory()?;
        Ok(Self {
            directory,
            root_session,
            records: Arc::new(Mutex::new(AnchoredRecordRegistry::default())),
            alias_inventory: Arc::new(Mutex::new(None)),
            test_path: Some(path.to_path_buf()),
        })
    }

    pub(crate) fn identity(&self) -> io::Result<DirectoryIdentity> {
        self.directory.identity()
    }

    #[cfg(test)]
    pub(crate) fn admitted_record_count(&self) -> usize {
        self.records
            .lock()
            .expect("anchored record directory registry lock poisoned")
            .records
            .values()
            .filter(|record| record.admitted.is_some())
            .count()
    }

    #[cfg(test)]
    pub(crate) fn peak_admitted_record_count(&self) -> usize {
        self.records
            .lock()
            .expect("anchored record directory registry lock poisoned")
            .peak_admitted
    }

    pub(crate) fn effect_owner(&self) -> io::Result<EffectOwner> {
        self.directory.create_effect_owner()
    }

    pub(crate) fn target(
        &self,
        name: &OsStr,
        max_existing_bytes: u64,
    ) -> io::Result<AnchoredRecordTarget> {
        if max_existing_bytes == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored record byte bound must be positive",
            ));
        }
        let leaf = capability_leaf(name)?;
        let mutation = self.mutation_for(&leaf)?;
        Ok(AnchoredRecordTarget {
            directory: self.clone(),
            leaf,
            max_existing_bytes,
            mutation,
        })
    }

    fn mutation_for(
        &self,
        leaf: &LeafName,
    ) -> io::Result<Arc<Mutex<AnchoredRecordMutationState>>> {
        let mut records = self
            .records
            .lock()
            .expect("anchored record directory registry lock poisoned");
        if let Some(mutation) = records.lookup(leaf)? {
            return Ok(mutation);
        }
        drop(records);
        self.ensure_portable_alias_absent(leaf)?;
        let mut records = self
            .records
            .lock()
            .expect("anchored record directory registry lock poisoned");
        if let Some(mutation) = records.lookup(leaf)? {
            return Ok(mutation);
        }
        let mutation = Arc::new(Mutex::new(AnchoredRecordMutationState::default()));
        records.insert(leaf.clone(), &mutation)?;
        Ok(mutation)
    }

    fn retain_admitted_mutation(
        &self,
        leaf: &LeafName,
        mutation: &Arc<Mutex<AnchoredRecordMutationState>>,
    ) {
        self.records
            .lock()
            .expect("anchored record directory registry lock poisoned")
            .retain_admitted(leaf, mutation);
    }

    fn ensure_portable_alias_absent(&self, leaf: &LeafName) -> io::Result<DirectoryRevision> {
        let current = self.directory.revision()?;
        if let Some(inventory) = self
            .alias_inventory
            .lock()
            .expect("anchored record alias inventory lock poisoned")
            .as_ref()
            .filter(|inventory| inventory.revision == current)
        {
            ensure_alias_absent_in_names(&inventory.names, leaf)?;
            return Ok(current);
        }
        for _ in 0..ALIAS_VALIDATION_ATTEMPTS {
            let before = self.directory.revision()?;
            let listing = self.directory.entries(MAX_DIRECTORY_ENTRIES)?;
            let after = self.directory.revision()?;
            if before != after {
                continue;
            }
            if listing.state() == DirectoryListingState::Truncated {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored record directory exceeds its alias validation bound",
                ));
            }
            let mut names = HashMap::<LeafNameEquivalenceKey, Vec<OsString>>::new();
            for entry in listing.entries() {
                let name = entry.name().to_os_string();
                for key in leaf_name_equivalence_keys(&name) {
                    names.entry(key).or_default().push(name.clone());
                }
            }
            ensure_alias_absent_in_names(&names, leaf)?;
            *self
                .alias_inventory
                .lock()
                .expect("anchored record alias inventory lock poisoned") =
                Some(AnchoredRecordAliasInventory {
                    revision: after,
                    names,
                });
            return Ok(after);
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "anchored record directory changed during alias validation",
        ))
    }

    fn ensure_fresh_portable_alias_absent(
        &self,
        leaf: &LeafName,
    ) -> io::Result<DirectoryRevision> {
        *self
            .alias_inventory
            .lock()
            .expect("anchored record alias inventory lock poisoned") = None;
        self.ensure_portable_alias_absent(leaf)
    }

    fn release_mutation(
        &self,
        leaf: &LeafName,
        mutation: &Arc<Mutex<AnchoredRecordMutationState>>,
    ) {
        self.records
            .lock()
            .expect("anchored record directory registry lock poisoned")
            .release(leaf, mutation);
    }

    pub(crate) fn names_bounded(&self, max_entries: usize) -> io::Result<Option<Vec<OsString>>> {
        let listing_limit = max_entries.saturating_add(1).clamp(1, MAX_DIRECTORY_ENTRIES);
        let listing = self.directory.entries(listing_limit)?;
        if listing.state() == DirectoryListingState::Truncated
            || listing.entries().len() > max_entries
        {
            return Ok(None);
        }
        Ok(Some(
            listing
                .entries()
                .iter()
                .map(|entry| entry.name().to_os_string())
                .collect(),
        ))
    }

    pub(crate) fn epoch(&self) -> io::Result<AnchoredRecordDirectoryEpoch> {
        self.directory
            .revision()
            .map(AnchoredRecordDirectoryEpoch)
    }

    pub(crate) fn read(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordObservation> {
        self.read_inner(name, max_bytes)
    }

    pub(crate) fn digest(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordDigestObservation> {
        let leaf = capability_leaf(name)?;
        let file = self.directory.open_file(&leaf)?;
        let revision = file.revision()?;
        if revision.size() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record exceeds its digest bound",
            ));
        }
        let size = revision.size();
        let modified_at_ns = revision.modified_at_ns()?;
        let mut sha256_hasher = Sha256::new();
        let mut sha512_hasher = Sha512::new();
        let mut reader = file.reader(max_bytes)?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            sha256_hasher.update(&buffer[..read]);
            sha512_hasher.update(&buffer[..read]);
        }
        reader.finish()?;
        file.validate_revision(&revision)?;
        let sha256 = sha256_hasher.finalize().into();
        let sha512 = sha512_hasher.finalize().into();
        let identity = AnchoredRecordIdentity {
            directory: self.clone(),
            file,
            leaf: leaf.clone(),
            revision,
            quarantine_sha256: Some(sha256),
        };
        identity.revalidate()?;
        Ok(AnchoredRecordDigestObservation {
            sha256,
            sha512,
            size,
            modified_at_ns,
            identity,
        })
    }

    fn read_inner(&self, name: &OsStr, max_bytes: u64) -> io::Result<AnchoredRecordObservation> {
        let leaf = capability_leaf(name)?;
        let file = self.directory.open_file(&leaf)?;
        let revision = file.revision()?;
        if revision.size() > max_bytes {
            let identity = AnchoredRecordIdentity {
                directory: self.clone(),
                file,
                leaf,
                revision,
                quarantine_sha256: None,
            };
            identity.revalidate()?;
            return Ok(AnchoredRecordObservation::Oversized { identity });
        }
        let bytes = file.read_bounded(max_bytes)?;
        file.validate_revision(&revision)?;
        let sha256 = Sha256::digest(&bytes).into();
        let identity = AnchoredRecordIdentity {
            directory: self.clone(),
            file,
            leaf: leaf.clone(),
            revision,
            quarantine_sha256: Some(sha256),
        };
        identity.revalidate()?;
        Ok(AnchoredRecordObservation::Bytes { bytes, identity })
    }

}

impl AnchoredRecordTarget {
    fn admit_published(
        &self,
        file: FileCapability,
        revision: FileRevision,
        sha256: [u8; 32],
        size: u64,
    ) -> io::Result<()> {
        if size > self.max_existing_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record generation exceeds its byte bound",
            ));
        }
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        if let Some(published) = mutation.published.as_ref() {
            let same = published.sha256 == sha256
                && published.size == size
                && published.file.same_file(&file)?
                && published.file.validate_revision(&published.revision).is_ok()
                && file.validate_revision(&revision).is_ok();
            if !same {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "another anchored record generation is already admitted",
                ));
            }
            return Ok(());
        }
        if mutation.pending_replace.is_some()
            || mutation.delete.is_some()
            || mutation.source_latched
            || mutation.terminal
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "anchored record generation cannot be admitted during mutation",
            ));
        }
        mutation.published = Some(PublishedRecord {
            file,
            revision,
            sha256,
            size,
        });
        mutation.source_latched = true;
        drop(mutation);
        self.directory
            .retain_admitted_mutation(&self.leaf, &self.mutation);
        Ok(())
    }

    pub(crate) fn directory(&self) -> AnchoredRecordDirectory {
        self.directory.clone()
    }

    pub(crate) fn directory_identity(&self) -> io::Result<DirectoryIdentity> {
        self.directory.identity()
    }

    pub(crate) fn leaf(&self) -> &LeafName {
        &self.leaf
    }

    pub(crate) fn max_existing_bytes(&self) -> u64 {
        self.max_existing_bytes
    }

    pub(crate) fn write(&self, effects: &EffectOwner, contents: &[u8]) -> io::Result<()> {
        self.settle(effects)?;
        self.require_alias_free()?;
        let mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        if mutation.terminal || mutation.delete.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "anchored record generation is deleted or deletion remains pending",
            ));
        }
        drop(mutation);
        let expected_sha256 = <[u8; 32]>::from(Sha256::digest(contents));
        let expected_size = u64::try_from(contents.len())
            .map_err(|_| io::Error::other("anchored record size does not fit u64"))?;
        if expected_size > self.max_existing_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record write exceeds its byte bound",
            ));
        }
        if self.current_matches(expected_sha256, expected_size)? {
            return Ok(());
        }

        let mut staged = settle_stage_create(self.directory.directory.create_stage(), effects)?;
        if let Err(error) = staged.write_all(contents) {
            discard_stage(staged, effects)?;
            return Err(error);
        }
        let sealed = match staged.seal() {
            Ok(sealed) => sealed,
            Err(failure) => {
                let error = copy_io_error(failure.error());
                discard_stage(failure.into_staged(), effects)?;
                return Err(error);
            }
        };
        let destination = match self.replace_destination() {
            Ok(destination) => destination,
            Err(error) => {
                discard_sealed_stage(sealed, effects)?;
                return Err(error);
            }
        };
        match sealed.replace_nondurable(destination) {
            FileReplaceOutcome::Replaced { current, displaced } => {
                let result = self.finish_replacement(
                    effects,
                    current,
                    displaced,
                    expected_sha256,
                    expected_size,
                );
                self.record_alias_postcheck();
                result
            }
            FileReplaceOutcome::NoEffect {
                error,
                staged,
                destination,
            } => {
                drop(destination);
                discard_sealed_stage(staged, effects)?;
                Err(error)
            }
            FileReplaceOutcome::AppliedUnverified(obligation) => {
                let receipt = retain_linear(
                    effects,
                    obligation,
                    EffectOwner::retain_file_replace,
                );
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .pending_replace = Some(PendingRecordReplace {
                    receipt,
                    sha256: expected_sha256,
                    size: expected_size,
                });
                self.settle(effects)?;
                if self.current_matches(expected_sha256, expected_size)? {
                    self.record_alias_postcheck();
                    Ok(())
                } else {
                    Err(io::Error::other(
                        "anchored record replacement settled without the expected content",
                    ))
                }
            }
        }
    }

    pub(crate) fn remove(&self, effects: &EffectOwner) -> io::Result<()> {
        self.settle(effects)?;
        self.require_alias_free()?;
        if self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .terminal
        {
            return Ok(());
        }
        let delete = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .delete
            .take();
        let request = match delete {
            Some(AnchoredRecordDeleteState::Source(request)) => request,
            Some(AnchoredRecordDeleteState::Park(obligation)) => {
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .delete = Some(AnchoredRecordDeleteState::Park(obligation));
                return self.settle_pending_delete(effects);
            }
            Some(AnchoredRecordDeleteState::Retired) => {
                settle_effects_complete(effects)?;
                self.mark_delete_retired();
                return self.finish_retired_delete();
            }
            None => match self.current_source()? {
                AnchoredRecordSource::Current(request) => request,
                AnchoredRecordSource::Vacant | AnchoredRecordSource::Displaced => {
                    self.clear_delete_state();
                    return Ok(());
                }
            },
        };
        let parked = match self.directory.directory.park_file(request) {
            FileParkOutcome::Parked(parked) => parked,
            FileParkOutcome::NoEffect { error, request } => {
                return self.finish_no_effect_delete(error, request);
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .delete = Some(AnchoredRecordDeleteState::Park(obligation));
                return self.settle_pending_delete(effects);
            }
        };
        self.mark_delete_retired();
        remove_parked_file(effects, parked)?;
        settle_effects_complete(effects)?;
        self.finish_retired_delete()
    }

    pub(crate) fn settle(&self, effects: &EffectOwner) -> io::Result<()> {
        effects.settle()?;
        self.settle_pending_replace(effects)?;
        self.settle_pending_delete(effects)
    }

    fn settle_pending_replace(&self, effects: &EffectOwner) -> io::Result<()> {
        let pending = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .pending_replace
            .take();
        let Some(pending) = pending else {
            return Ok(());
        };
        match pending.receipt.claim() {
            FileReplaceReceiptOutcome::Pending(receipt) => {
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .pending_replace = Some(PendingRecordReplace {
                    receipt,
                    sha256: pending.sha256,
                    size: pending.size,
                });
                Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "anchored record replacement remains unsettled",
                ))
            }
            FileReplaceReceiptOutcome::Replaced { current, displaced } => {
                let result = self.finish_replacement(
                    effects,
                    current,
                    displaced,
                    pending.sha256,
                    pending.size,
                );
                self.record_alias_postcheck();
                result
            }
            FileReplaceReceiptOutcome::NoEffect {
                staged,
                destination,
            } => {
                drop(destination);
                discard_sealed_stage(staged, effects)?;
                Err(io::Error::other(
                    "anchored record replacement had no effect",
                ))
            }
        }
    }

    fn settle_pending_delete(&self, effects: &EffectOwner) -> io::Result<()> {
        let pending = {
            let mut mutation = self
                .mutation
                .lock()
                .expect("anchored record mutation lock poisoned");
            match mutation.delete.take() {
                Some(AnchoredRecordDeleteState::Park(obligation)) => Some(obligation),
                other => {
                    mutation.delete = other;
                    None
                }
            }
        };
        let Some(obligation) = pending else {
            return Ok(());
        };
        let original_error = copy_io_error(obligation.error());
        match obligation.reconcile() {
            FileParkResolution::Parked(parked) => {
                self.mark_delete_retired();
                remove_parked_file(effects, parked)?;
                settle_effects_complete(effects)?;
                self.finish_retired_delete()
            }
            FileParkResolution::NoEffect(request) => {
                self.finish_no_effect_delete(original_error, request)
            }
            FileParkResolution::Indeterminate(obligation) => {
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .delete = Some(AnchoredRecordDeleteState::Park(obligation));
                Err(original_error)
            }
        }
    }

    fn finish_replacement(
        &self,
        effects: &EffectOwner,
        current: FileCapability,
        displaced: Option<ParkedFile>,
        sha256: [u8; 32],
        size: u64,
    ) -> io::Result<()> {
        let revision = current.revision()?;
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        mutation.published = Some(PublishedRecord {
            file: current,
            revision,
            sha256,
            size,
        });
        mutation.source_latched = true;
        drop(mutation);
        self.directory
            .retain_admitted_mutation(&self.leaf, &self.mutation);
        if let Some(displaced) = displaced {
            remove_parked_file(effects, displaced)?;
        }
        Ok(())
    }

    fn current_matches(&self, sha256: [u8; 32], size: u64) -> io::Result<bool> {
        let mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        if let Some(published) = mutation.published.as_ref() {
            if published.file.validate_revision(&published.revision).is_ok() {
                return Ok(published.sha256 == sha256 && published.size == size);
            }
        }
        if mutation.source_latched {
            return Ok(false);
        }
        drop(mutation);
        let Some(observed) = self.observe_current()? else {
            return Ok(false);
        };
        let matches = observed.sha256 == sha256 && observed.size == size;
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        mutation.published = Some(observed);
        mutation.source_latched = true;
        drop(mutation);
        if matches {
            self.directory
                .retain_admitted_mutation(&self.leaf, &self.mutation);
        }
        Ok(matches)
    }

    fn replace_destination(&self) -> io::Result<ReplaceDestination> {
        self.require_alias_free()?;
        match self.current_source()? {
            AnchoredRecordSource::Current(request) => Ok(ReplaceDestination::Existing(request)),
            AnchoredRecordSource::Vacant => Ok(ReplaceDestination::Vacant {
                parent: self.directory.directory.clone(),
                name: self.leaf.clone(),
            }),
            AnchoredRecordSource::Displaced => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "anchored record source generation was replaced",
            )),
        }
    }

    fn current_source(&self) -> io::Result<AnchoredRecordSource> {
        let (published, source_latched) = {
            let mut mutation = self
                .mutation
                .lock()
                .expect("anchored record mutation lock poisoned");
            (mutation.published.take(), mutation.source_latched)
        };
        if let Some(published) = published
            && published.file.validate_revision(&published.revision).is_ok()
        {
            return Ok(AnchoredRecordSource::Current(published.file.park_request(
                ExpectedFileContent::new(
                published.revision,
                published.sha256,
            ))));
        }
        if source_latched {
            return Ok(AnchoredRecordSource::Displaced);
        }
        let Some(published) = self.observe_current()? else {
            return Ok(AnchoredRecordSource::Vacant);
        };
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        mutation.published = Some(published);
        mutation.source_latched = true;
        drop(mutation);
        self.current_source()
    }

    fn mark_delete_retired(&self) {
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        mutation.published = None;
        mutation.delete = Some(AnchoredRecordDeleteState::Retired);
    }

    fn clear_delete_state(&self) {
        let mut mutation = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned");
        mutation.published = None;
        mutation.delete = None;
        mutation.terminal = true;
        drop(mutation);
        self.directory.release_mutation(&self.leaf, &self.mutation);
    }

    fn finish_retired_delete(&self) -> io::Result<()> {
        self.clear_delete_state();
        Ok(())
    }

    fn require_alias_free(&self) -> io::Result<()> {
        let latched = self
            .mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .alias_latched;
        if latched {
            *self
                .directory
                .alias_inventory
                .lock()
                .expect("anchored record alias inventory lock poisoned") = None;
        }
        let result = self.directory.ensure_portable_alias_absent(&self.leaf);
        self.mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .alias_latched = result.is_err();
        result.map(|_| ())
    }

    fn record_alias_postcheck(&self) {
        *self
            .directory
            .alias_inventory
            .lock()
            .expect("anchored record alias inventory lock poisoned") = None;
        let failed = self
            .directory
            .ensure_portable_alias_absent(&self.leaf)
            .is_err();
        self.mutation
            .lock()
            .expect("anchored record mutation lock poisoned")
            .alias_latched = failed;
    }

    fn finish_no_effect_delete(
        &self,
        park_error: io::Error,
        request: axial_fs::FileParkRequest,
    ) -> io::Result<()> {
        match request.classify_source(&self.directory.directory) {
            Ok(FileParkRequestSource::Displaced) => {
                self.clear_delete_state();
                Ok(())
            }
            Ok(FileParkRequestSource::Current(request)) => {
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .delete = Some(AnchoredRecordDeleteState::Source(request));
                Err(park_error)
            }
            Err(failure) => {
                let (probe_error, request) = failure.into_parts();
                self.mutation
                    .lock()
                    .expect("anchored record mutation lock poisoned")
                    .delete = Some(AnchoredRecordDeleteState::Source(request));
                Err(io::Error::new(
                    park_error.kind(),
                    format!(
                        "{park_error}; exact source currentness check failed: {probe_error}"
                    ),
                ))
            }
        }
    }

    fn observe_current(&self) -> io::Result<Option<PublishedRecord>> {
        let file = match self.directory.directory.open_file(&self.leaf) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let revision = file.revision()?;
        if revision.size() > self.max_existing_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record exceeds its byte bound",
            ));
        }
        let size = revision.size();
        let mut reader = file.reader(self.max_existing_bytes)?;
        let mut hasher = Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let read = reader.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        reader.finish()?;
        file.validate_revision(&revision)?;
        Ok(Some(PublishedRecord {
            file,
            revision,
            sha256: hasher.finalize().into(),
            size,
        }))
    }

    #[cfg(test)]
    pub(crate) fn test_path(&self) -> PathBuf {
        self.directory
            .test_path
            .as_ref()
            .map_or_else(|| PathBuf::from(self.leaf.as_os_str()), |directory| {
                directory.join(self.leaf.as_os_str())
            })
    }
}

impl Drop for AnchoredRecordTarget {
    fn drop(&mut self) {
        if Arc::strong_count(&self.mutation) == 1 {
            self.directory.release_mutation(&self.leaf, &self.mutation);
        }
    }
}

fn settle_stage_create(outcome: FileCreateOutcome, effects: &EffectOwner) -> io::Result<axial_fs::StagedFile> {
    match outcome {
        FileCreateOutcome::Created(staged) => Ok(staged),
        FileCreateOutcome::NoEffect(error) => Err(error),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(staged) => Ok(staged),
            FileCreateResolution::Indeterminate(obligation) => {
                let error = copy_io_error(obligation.error());
                retain_linear(
                    effects,
                    obligation,
                    EffectOwner::retain_stage_create_cleanup,
                );
                effects.settle()?;
                Err(error)
            }
        },
    }
}

fn discard_stage(staged: axial_fs::StagedFile, effects: &EffectOwner) -> io::Result<()> {
    match staged.discard() {
        StageDiscardOutcome::Discarded => Ok(()),
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            retain_linear(effects, obligation, EffectOwner::retain_stage_discard);
            effects.settle()
        }
    }
}

fn discard_sealed_stage(staged: SealedStagedFile, effects: &EffectOwner) -> io::Result<()> {
    match staged.discard() {
        StageDiscardOutcome::Discarded => Ok(()),
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            retain_linear(effects, obligation, EffectOwner::retain_stage_discard);
            effects.settle()
        }
    }
}

fn remove_parked_file(effects: &EffectOwner, parked: ParkedFile) -> io::Result<()> {
    match parked.remove() {
        FileRemovalOutcome::Removed => Ok(()),
        FileRemovalOutcome::NoEffect { error, parked } => {
            retain_linear(
                effects,
                parked,
                EffectOwner::retain_parked_file_removal,
            );
            effects.settle().map_err(|settlement| {
                io::Error::new(settlement.kind(), format!("{error}; {settlement}"))
            })
        }
        FileRemovalOutcome::AppliedUnverified(obligation) => {
            retain_linear(effects, obligation, EffectOwner::retain_file_removal);
            effects.settle()
        }
    }
}

fn settle_effects_complete(effects: &EffectOwner) -> io::Result<()> {
    effects.settle()?;
    effects.require_settled()
}

fn retain_linear<T, R>(
    effects: &EffectOwner,
    carrier: T,
    retain: impl Fn(&EffectOwner, T) -> Result<R, axial_fs::EffectOwnerRetentionError<T>>,
) -> R {
    let (error, carrier) = match retain(effects, carrier) {
        Ok(retained) => return retained,
        Err(failure) => failure.into_parts(),
    };
    if error.kind() != io::ErrorKind::WouldBlock {
        fail_stop_linear(carrier);
    }
    let _ = effects.settle();
    match retain(effects, carrier) {
        Ok(retained) => retained,
        Err(failure) => {
            let (_, carrier) = failure.into_parts();
            fail_stop_linear(carrier)
        }
    }
}

fn fail_stop_linear<T>(carrier: T) -> ! {
    let _carrier = carrier;
    std::process::abort()
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

impl AnchoredRecordDigestObservation {
    pub(crate) fn parts(&self) -> ([u8; 32], [u8; 64], u64, u64) {
        (self.sha256, self.sha512, self.size, self.modified_at_ns)
    }

    pub(crate) fn revalidate(&self) -> io::Result<()> {
        self.identity.revalidate()
    }
}

impl AnchoredRecordIdentity {
    pub(crate) fn revalidate(&self) -> io::Result<()> {
        self.file.validate_revision(&self.revision)
    }

    fn admit(self, max_existing_bytes: u64) -> io::Result<AnchoredRecordTarget> {
        let Some(sha256) = self.quarantine_sha256 else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized anchored record cannot be admitted",
            ));
        };
        self.revalidate()?;
        let size = self.revision.size();
        if size > max_existing_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record admission exceeds its byte bound",
            ));
        }
        let target = self
            .directory
            .target(self.leaf.as_os_str(), max_existing_bytes)?;
        target.admit_published(
            self.file,
            self.revision,
            sha256,
            size,
        )?;
        Ok(target)
    }

    pub(crate) fn quarantine(
        self,
        suffix: [u8; 16],
    ) -> Result<AnchoredRecordQuarantineReceipt, AnchoredRecordQuarantineError> {
        let Some(sha256) = self.quarantine_sha256 else {
            return Err(AnchoredRecordQuarantineError::Refused(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized anchored records are ineligible for quarantine",
            )));
        };
        let destination = match capability_leaf(&anchored_record_quarantine_name(
            self.leaf.as_os_str(),
            suffix,
        )) {
            Ok(destination) => destination,
            Err(error) => return Err(AnchoredRecordQuarantineError::Refused(error)),
        };
        self.directory
            .ensure_fresh_portable_alias_absent(&self.leaf)
            .map_err(AnchoredRecordQuarantineError::Refused)?;
        self.directory
            .ensure_portable_alias_absent(&destination)
            .map_err(AnchoredRecordQuarantineError::Refused)?;
        self.file
            .validate_revision(&self.revision)
            .map_err(AnchoredRecordQuarantineError::Refused)?;
        let request = self
            .file
            .park_request(ExpectedFileContent::new(self.revision, sha256));
        settle_capability_park(
            self.directory
                .directory
                .park_file_as(request, destination.clone()),
            self.directory,
            self.leaf,
            destination,
        )
    }

    pub(crate) fn admit_existing_quarantine(
        self,
        original_name: &OsStr,
    ) -> io::Result<AnchoredRecordQuarantineReceipt> {
        let Some(sha256) = self.quarantine_sha256 else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "oversized anchored records are ineligible for quarantine admission",
            ));
        };
        let original = capability_leaf(original_name)?;
        self.directory
            .ensure_fresh_portable_alias_absent(&self.leaf)?;
        self.directory.ensure_portable_alias_absent(&original)?;
        self.file.validate_revision(&self.revision)?;
        let request = self
            .file
            .park_request(ExpectedFileContent::new(self.revision, sha256));
        self.directory
            .directory
            .admit_existing_file_park(&original, request)
            .map(|parked| AnchoredRecordQuarantineReceipt {
                parked,
                directory: self.directory,
                original,
                parked_leaf: self.leaf,
            })
    }

}

impl AnchoredRecordQuarantineReceipt {
    pub(crate) fn is_current(&self) -> bool {
        self.aliases_are_current().is_ok() && self.parked.validate_current().is_ok()
    }

    pub(crate) fn acknowledge_preserved(
        self,
    ) -> Result<(), AnchoredRecordQuarantinePreservationError> {
        if let Err(error) = self.aliases_are_current() {
            return Err(AnchoredRecordQuarantinePreservationError::Alias {
                error,
                _receipt: self,
            });
        }
        let AnchoredRecordQuarantineReceipt {
            parked,
            directory,
            ..
        } = self;
        parked.acknowledge_preserved().map_err(|error| {
            AnchoredRecordQuarantinePreservationError::Acknowledgement {
                error,
                _directory: directory,
            }
        })
    }

    pub(crate) fn acknowledge_applied_unverified(
        self,
    ) -> Option<AnchoredRecordQuarantinePreservationError> {
        if let Err(error) = self.aliases_are_current() {
            return Some(AnchoredRecordQuarantinePreservationError::Alias {
                error,
                _receipt: self,
            });
        }
        let AnchoredRecordQuarantineReceipt {
            parked,
            directory,
            ..
        } = self;
        parked
            .acknowledge_preserved()
            .err()
            .map(|error| AnchoredRecordQuarantinePreservationError::Acknowledgement {
                error,
                _directory: directory,
            })
    }

    fn aliases_are_current(&self) -> io::Result<()> {
        self.directory
            .ensure_fresh_portable_alias_absent(&self.original)?;
        self.directory
            .ensure_portable_alias_absent(&self.parked_leaf)?;
        Ok(())
    }
}

impl std::fmt::Debug for AnchoredRecordQuarantinePreservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AnchoredRecordQuarantinePreservationError")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for AnchoredRecordQuarantinePreservationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acknowledgement { .. } => formatter
                .write_str("anchored record quarantine preservation could not be acknowledged"),
            Self::Alias { .. } => formatter
                .write_str("anchored record quarantine portable name proof is not current"),
            Self::IndeterminatePark { .. } => formatter
                .write_str("anchored record quarantine preservation remains indeterminate"),
        }
    }
}

impl std::error::Error for AnchoredRecordQuarantinePreservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Acknowledgement { error, .. } => Some(error.error()),
            Self::Alias { error, .. } => Some(error),
            Self::IndeterminatePark { obligation, .. } => Some(obligation.error()),
        }
    }
}

impl AnchoredRecordQuarantineError {
    pub(crate) fn into_preservation_error(
        self,
    ) -> Option<AnchoredRecordQuarantinePreservationError> {
        match self {
            Self::Refused(_) => None,
            Self::AppliedUnverified {
                obligation,
                _root_session,
            } => Some(AnchoredRecordQuarantinePreservationError::IndeterminatePark {
                obligation,
                _root_session,
            }),
        }
    }
}

fn settle_capability_park(
    outcome: FileParkOutcome,
    directory: AnchoredRecordDirectory,
    original: LeafName,
    parked_leaf: LeafName,
) -> Result<AnchoredRecordQuarantineReceipt, AnchoredRecordQuarantineError> {
    match outcome {
        FileParkOutcome::Parked(parked) => Ok(AnchoredRecordQuarantineReceipt {
            parked,
            directory,
            original,
            parked_leaf,
        }),
        FileParkOutcome::NoEffect { error, .. } => {
            Err(AnchoredRecordQuarantineError::Refused(error))
        }
        FileParkOutcome::AppliedUnverified(obligation) => {
            let error = io::Error::new(obligation.error().kind(), obligation.error().to_string());
            match obligation.reconcile() {
                FileParkResolution::Parked(parked) => Ok(AnchoredRecordQuarantineReceipt {
                    parked,
                    directory,
                    original,
                    parked_leaf,
                }),
                FileParkResolution::NoEffect(_) => {
                    Err(AnchoredRecordQuarantineError::Refused(error))
                }
                FileParkResolution::Indeterminate(obligation) => {
                    Err(AnchoredRecordQuarantineError::AppliedUnverified {
                        obligation,
                        _root_session: Arc::clone(&directory.root_session),
                    })
                }
            }
        }
    }
}

fn capability_leaf(name: &OsStr) -> io::Result<LeafName> {
    LeafName::new(name.to_os_string()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "anchored record name is not a direct native leaf",
        )
    })
}

fn ensure_alias_absent_in_names(
    names: &HashMap<LeafNameEquivalenceKey, Vec<OsString>>,
    leaf: &LeafName,
) -> io::Result<()> {
    let alias_exists = leaf_name_equivalence_keys(leaf.as_os_str())
        .iter()
        .filter_map(|key| names.get(key))
        .flatten()
        .any(|name| {
            name.as_os_str() != leaf.as_os_str()
                && leaf_names_equivalent(name.as_os_str(), leaf.as_os_str())
        });
    if alias_exists {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "portable-equivalent anchored record already exists",
        ));
    }
    Ok(())
}

#[cfg(unix)]
fn update_native_name(hasher: &mut Sha256, name: &LeafName) {
    use std::os::unix::ffi::OsStrExt as _;

    let name = name.as_os_str();
    let bytes = name.as_bytes();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

#[cfg(windows)]
fn update_native_name(hasher: &mut Sha256, name: &LeafName) {
    use std::os::windows::ffi::OsStrExt as _;

    let name = name.as_os_str();
    let units = name.encode_wide().collect::<Vec<_>>();
    hasher.update((units.len() as u64).to_le_bytes());
    for unit in units {
        hasher.update(unit.to_le_bytes());
    }
}

#[cfg(not(any(unix, windows)))]
fn update_native_name(hasher: &mut Sha256, name: &LeafName) {
    let bytes = name.as_os_str().to_string_lossy();
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes.as_bytes());
}

pub(crate) fn anchored_record_quarantine_name(canonical: &OsStr, suffix: [u8; 16]) -> OsString {
    let mut destination = OsString::from(".");
    destination.push(canonical);
    destination.push(".axial-quarantine-");
    let mut encoded = String::with_capacity(32);
    for byte in suffix {
        use std::fmt::Write as _;
        write!(&mut encoded, "{byte:02x}").expect("writing to a String cannot fail");
    }
    destination.push(encoded);
    destination
}
