use crate::loaders::types::LoaderError;
use crate::portable_path::{PortableFileName, PortablePathKey, PortableRelativePath};
use axial_fs::{
    AdmittedAbsoluteDirectory, AdmittedRootSession, AdmittedRootSessionAcquireOutcome, Directory,
    DirectoryCreateOutcome, DirectoryEntry, DirectoryIdentity, DirectoryListingState,
    DirectoryMoveOutcome, DirectoryMoveReceipt, DirectoryMoveReceiptOutcome, DirectoryParkOutcome,
    DirectoryRemovalOutcome, EffectOwner, EntryKind, ExpectedFileContent, FileCapability,
    FileCreateOutcome, FileMoveOutcome, FileMoveReceipt, FileMoveReceiptOutcome, FileParkOutcome,
    FilePromotionOutcome, FilePromotionReceipt, FilePromotionReceiptOutcome, FileRemovalOutcome,
    FileReplaceOutcome, FileReplaceReceipt, FileReplaceReceiptOutcome, LeafName, ParkedDirectory,
    ParkedFile, ReplaceDestination, RootSession, RootSessionAcquireOutcome, SealedStagedFile,
    StageDiscardOutcome, StagedFile,
};
use sha1::{Digest as _, Sha1};
use sha2::{Sha256, Sha512};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::ffi::OsString;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};

pub(crate) const MAX_MANAGED_TEMP_ENTRIES: usize = 128;
pub(crate) const MAX_MANAGED_DIRECTORY_ENTRIES: usize = 4096;
const MAX_MANAGED_READ_BYTES: u64 = 512 << 20;
const MAX_MANAGED_TREE_ENTRIES: usize = MAX_MANAGED_DIRECTORY_ENTRIES;
const MAX_MANAGED_TREE_DEPTH: usize = 16;
const MAX_MANAGED_TREE_FILE_BYTES: u64 = 128 << 20;
const MAX_MANAGED_TREE_TOTAL_BYTES: u64 = 512 << 20;
const MAX_MANAGED_TREE_OPERATION_ENTRIES: usize = 100_000;
const MAX_MANAGED_TREE_OPERATION_DEPTH: usize = 64;
const MAX_MANAGED_TREE_NAME_CANDIDATES: usize = 100;
const MAX_MANAGED_EFFECT_CONTINUATIONS: usize = 256;
const ROOT_LEASE_NAME: &str = ".axial-root.lease";

#[cfg(test)]
type ManagedSha1ReadIdentity = (u64, [u8; 20]);

#[cfg(test)]
type ManagedSha1ReadCounts = HashMap<ManagedSha1ReadIdentity, usize>;

#[cfg(test)]
type ManagedSha1ReadScopes = HashMap<PathBuf, ManagedSha1ReadCounts>;

#[cfg(test)]
static SHA1_FULL_READ_COUNTS: OnceLock<Mutex<ManagedSha1ReadScopes>> = OnceLock::new();

#[cfg(test)]
#[derive(Default)]
pub(crate) struct ManagedSha1FullReadCounts {
    counts: ManagedSha1ReadCounts,
}

#[cfg(test)]
impl ManagedSha1FullReadCounts {
    pub(crate) fn count(&self, size: u64, sha1: [u8; 20]) -> usize {
        self.counts.get(&(size, sha1)).copied().unwrap_or_default()
    }
}

#[cfg(test)]
pub(crate) fn register_sha1_full_read_counts(root: &Path) {
    let replaced = SHA1_FULL_READ_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("managed SHA-1 full-read counter registry")
        .insert(root.to_path_buf(), HashMap::new());
    assert!(replaced.is_none(), "managed SHA-1 full-read scope reused");
}

#[cfg(test)]
pub(crate) fn take_sha1_full_read_counts(root: &Path) -> ManagedSha1FullReadCounts {
    let counts = SHA1_FULL_READ_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("managed SHA-1 full-read counter registry")
        .remove(root)
        .expect("managed SHA-1 full-read scope was not registered");
    ManagedSha1FullReadCounts { counts }
}

#[cfg(test)]
fn record_sha1_full_read(directory: &Path, size: u64, sha1: [u8; 20]) {
    let mut scopes = SHA1_FULL_READ_COUNTS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .expect("managed SHA-1 full-read counter registry");
    for (root, counts) in scopes.iter_mut() {
        if directory.starts_with(root) {
            *counts.entry((size, sha1)).or_default() += 1;
        }
    }
}

#[derive(Clone)]
pub(crate) struct ManagedDir {
    inner: Arc<ManagedDirInner>,
}

struct ManagedDirInner {
    directory: Directory,
    identity: DirectoryIdentity,
    root: Arc<ManagedRoot>,
    operation_pin: Option<Arc<ManagedOperationPin>>,
    path: PathBuf,
    is_root: bool,
}

struct ManagedRoot {
    anchor: Directory,
    effects: EffectOwner,
    effect_transition: Mutex<()>,
    continuations: Mutex<ManagedEffectContinuations>,
    file_identities: Mutex<Vec<Weak<ManagedFileProof>>>,
    publication_locks: Mutex<HashMap<PublicationLockKey, Weak<PublicationLock>>>,
    publication_mutex: Arc<tokio::sync::Mutex<()>>,
    install_flights: Mutex<HashMap<PortablePathKey, Weak<tokio::sync::Mutex<()>>>>,
    _session: Option<ManagedRootSession>,
}

enum ManagedRootSession {
    Admitted(AdmittedRootSession),
    Direct(RootSession),
}

impl ManagedRootSession {
    fn validate_retained_authority(&self) -> io::Result<()> {
        match self {
            Self::Admitted(session) => session.validate_retained_authority(),
            Self::Direct(session) => session.validate_retained_authority(),
        }
    }
}

struct ManagedEffectContinuations {
    next_id: u64,
    receipts: BTreeMap<u64, ManagedEffectContinuation>,
}

enum ManagedEffectContinuation {
    FilePromotion(FilePromotionReceipt),
    FileReplace(FileReplaceReceipt),
    FileMove {
        receipt: FileMoveReceipt,
        identity: Arc<ManagedFileProof>,
    },
    DirectoryMove(DirectoryMoveReceipt),
    TreeDirectoryMove {
        receipt: DirectoryMoveReceipt,
        parent: ManagedDirDescriptor,
        stage_name: PortableFileName,
        stage: ManagedDirDescriptor,
    },
    TreeCleanup {
        parent: ManagedDirDescriptor,
        stage_name: PortableFileName,
        stage: ManagedDirDescriptor,
    },
}

struct ManagedDirDescriptor {
    directory: Directory,
    identity: DirectoryIdentity,
    path: PathBuf,
    is_root: bool,
}

struct ManagedEffectTransition<'a> {
    root: &'a Arc<ManagedRoot>,
    _guard: MutexGuard<'a, ()>,
}

struct ManagedFileProof {
    capability: Mutex<Option<FileCapability>>,
}

#[derive(Clone)]
pub(crate) struct ManagedFileIdentity {
    proof: Arc<ManagedFileProof>,
    _operation_pin: Option<Arc<ManagedOperationPin>>,
}

impl PartialEq for ManagedFileIdentity {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.proof, &other.proof)
    }
}

impl Eq for ManagedFileIdentity {}

impl std::fmt::Debug for ManagedFileIdentity {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedFileIdentity")
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ManagedDirectoryIdentity(DirectoryIdentity);

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct ManagedLibraryBinding(axial_fs::DirectoryFilesystemIdentity);

impl std::fmt::Debug for ManagedLibraryBinding {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryBinding")
            .finish_non_exhaustive()
    }
}

pub struct ManagedLibraryRoot {
    authority: Arc<ManagedLibraryAuthority>,
}

struct ManagedLibraryAuthority {
    root: ManagedDir,
    admission: Arc<ManagedLibraryAdmissionVerifier>,
    lifecycle: Arc<ManagedAuthorityLifecycle>,
}

struct ManagedLibraryAdmissionVerifier {
    state: RwLock<Arc<ManagedLibraryAdmissionState>>,
    root_identity: axial_fs::DirectoryFilesystemIdentity,
    test_root: Weak<ManagedRoot>,
}

struct ManagedLibraryAdmissionState {
    current: ManagedLibraryAdmission,
    epoch: u64,
}

enum ManagedLibraryAdmission {
    App(AdmittedAbsoluteDirectory),
    #[cfg(test)]
    Test { path: Arc<PathBuf> },
}

struct ManagedAuthorityLifecycle {
    state: Mutex<ManagedAuthorityLifecycleState>,
    active: tokio::sync::watch::Sender<usize>,
}

struct ManagedAuthorityLifecycleState {
    open: bool,
    active: usize,
}

struct ManagedOperationPin {
    lifecycle: Arc<ManagedAuthorityLifecycle>,
    admission: Option<Arc<ManagedLibraryAdmissionVerifier>>,
}

#[derive(Clone)]
pub struct ManagedLibraryOperation {
    authority: Arc<ManagedLibraryAuthority>,
    pin: Arc<ManagedOperationPin>,
}

#[derive(Clone)]
pub struct ManagedLibraryWitness {
    authority: Weak<ManagedLibraryAuthority>,
}

#[must_use = "retiring library authority must be drained and settled"]
pub struct ManagedLibraryRetirement {
    authority: Arc<ManagedLibraryAuthority>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagedLibraryRetirementBinding {
    BindingIntact,
    BindingLost,
}

#[must_use = "prepared library admission must be committed after persistence or dropped"]
pub struct PreparedManagedLibraryAdmissionRebind {
    operation: ManagedLibraryOperation,
    candidate: Option<AdmittedAbsoluteDirectory>,
    expected_epoch: u64,
}

#[must_use = "failed library admission rebind retains its candidate evidence"]
pub enum ManagedLibraryAdmissionRebindFailure {
    Stale(AdmittedAbsoluteDirectory),
    BindingLost(AdmittedAbsoluteDirectory),
    GenerationClosed(AdmittedAbsoluteDirectory),
}

impl std::fmt::Debug for ManagedLibraryRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryRoot")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedLibraryOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryOperation")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedLibraryWitness {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryWitness")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedLibraryRetirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedLibraryRetirement")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for PreparedManagedLibraryAdmissionRebind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PreparedManagedLibraryAdmissionRebind")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedLibraryAdmissionRebindFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct(match self {
                Self::Stale(_) => "ManagedLibraryAdmissionRebindFailure::Stale",
                Self::BindingLost(_) => "ManagedLibraryAdmissionRebindFailure::BindingLost",
                Self::GenerationClosed(_) => {
                    "ManagedLibraryAdmissionRebindFailure::GenerationClosed"
                }
            })
            .finish_non_exhaustive()
    }
}

impl ManagedLibraryAdmissionRebindFailure {
    pub fn into_candidate(self) -> AdmittedAbsoluteDirectory {
        match self {
            Self::Stale(candidate)
            | Self::BindingLost(candidate)
            | Self::GenerationClosed(candidate) => candidate,
        }
    }
}

impl Drop for ManagedLibraryRoot {
    fn drop(&mut self) {
        self.authority.close();
    }
}

impl Drop for ManagedOperationPin {
    fn drop(&mut self) {
        let mut state = self
            .lifecycle
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = state
            .active
            .checked_sub(1)
            .expect("managed authority operation count is balanced");
        self.lifecycle.active.send_replace(state.active);
    }
}

impl ManagedOperationPin {
    fn verify_admission(&self) -> io::Result<()> {
        match &self.admission {
            Some(admission) => admission.verify(),
            None => Ok(()),
        }
    }
}

impl ManagedAuthorityLifecycle {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(ManagedAuthorityLifecycleState {
                open: true,
                active: 0,
            }),
            active: tokio::sync::watch::channel(0).0,
        })
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.open = false;
        self.active.send_replace(state.active);
    }

    fn acquire_pin(
        self: &Arc<Self>,
        admission: Option<Arc<ManagedLibraryAdmissionVerifier>>,
    ) -> io::Result<Arc<ManagedOperationPin>> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("managed authority lifecycle lock was poisoned"))?;
        if !state.open {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "managed authority generation is retiring",
            ));
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("managed authority operation count overflowed"))?;
        self.active.send_replace(state.active);
        Ok(Arc::new(ManagedOperationPin {
            lifecycle: Arc::clone(self),
            admission,
        }))
    }

    fn is_drained(&self) -> io::Result<bool> {
        let state = self
            .state
            .lock()
            .map_err(|_| io::Error::other("managed authority lifecycle lock was poisoned"))?;
        if state.open {
            return Err(io::Error::other(
                "managed authority retirement has not fenced acquisition",
            ));
        }
        Ok(state.active == 0)
    }

    async fn drain(&self) -> io::Result<()> {
        let mut active = self.active.subscribe();
        loop {
            if *active.borrow_and_update() == 0 {
                return Ok(());
            }
            active.changed().await.map_err(|_| {
                io::Error::other("managed authority retirement witness was closed")
            })?;
        }
    }
}

fn verify_operation_admission(
    pin: &Option<Arc<ManagedOperationPin>>,
) -> io::Result<()> {
    match pin {
        Some(pin) => pin.verify_admission(),
        None => Ok(()),
    }
}

pub(crate) struct ManagedFileGuard {
    directory: Directory,
    name: LeafName,
    identity: ManagedFileIdentity,
    revision: axial_fs::FileRevision,
    size: u64,
    _operation_pin: Option<Arc<ManagedOperationPin>>,
}

#[derive(Clone)]
pub(crate) struct ManagedPassiveFileRevision(axial_fs::FileRevisionObservation);

impl std::fmt::Debug for ManagedFileGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedFileGuard")
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl ManagedFileGuard {
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn identity(&self) -> ManagedFileIdentity {
        self.identity.clone()
    }

    pub(crate) fn passive_revision(&self) -> ManagedPassiveFileRevision {
        ManagedPassiveFileRevision(self.revision.observation())
    }

    pub(crate) fn modified_at_ns(&self) -> Result<u64, LoaderError> {
        verify_operation_admission(&self._operation_pin)?;
        let modified_at_ns = self.revision.modified_at_ns()?;
        verify_operation_admission(&self._operation_pin)?;
        Ok(modified_at_ns)
    }

    pub(crate) fn into_bounded_reader(
        self,
        max_size: u64,
    ) -> Result<ManagedBoundedFileReader, LoaderError> {
        verify_operation_admission(&self._operation_pin)?;
        if self.size > max_size {
            return Err(LoaderError::Verify(
                "managed guarded reader exceeds its admitted bound".to_string(),
            ));
        }
        let file = self.directory.open_file(&self.name)?;
        if !self.identity.matches(&file)? {
            return Err(LoaderError::Verify(
                "managed guarded reader source changed before admission".to_string(),
            ));
        }
        let operation_pin = self._operation_pin;
        let reader = file.into_revision_reader(self.revision, max_size).map_err(|failure| {
                let (error, file, revision, _) = failure.into_parts();
                drop((file, revision));
                LoaderError::Io(error)
            })?;
        verify_operation_admission(&operation_pin)?;
        Ok(ManagedBoundedFileReader {
            reader,
            _operation_pin: operation_pin,
        })
    }
}

pub(crate) struct ManagedBoundedFileReader {
    reader: axial_fs::FileRevisionReader,
    _operation_pin: Option<Arc<ManagedOperationPin>>,
}

pub(crate) struct ManagedBoundedFileReaderFinishFailure {
    reader: Option<ManagedBoundedFileReader>,
}

impl Read for ManagedBoundedFileReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        verify_operation_admission(&self._operation_pin)?;
        let read = self.reader.read(bytes)?;
        verify_operation_admission(&self._operation_pin)?;
        Ok(read)
    }
}

impl Seek for ManagedBoundedFileReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        verify_operation_admission(&self._operation_pin)?;
        let position = self.reader.seek(position)?;
        verify_operation_admission(&self._operation_pin)?;
        Ok(position)
    }
}

impl ManagedBoundedFileReader {
    pub(crate) fn finish(
        self,
    ) -> Result<(), ManagedBoundedFileReaderFinishFailure> {
        if verify_operation_admission(&self._operation_pin).is_err() {
            return Err(ManagedBoundedFileReaderFinishFailure {
                reader: Some(self),
            });
        }
        let Self {
            reader,
            _operation_pin,
        } = self;
        match reader.finish() {
            Ok(file) => {
                drop(file);
                verify_operation_admission(&_operation_pin).map_err(|_| {
                    ManagedBoundedFileReaderFinishFailure { reader: None }
                })
            }
            Err(failure) => Err(ManagedBoundedFileReaderFinishFailure {
                reader: Some(Self {
                    reader: failure.into_reader(),
                    _operation_pin,
                }),
            }),
        }
    }

    pub(crate) fn cancel(self) {
        drop(self.reader.cancel());
    }
}

impl ManagedBoundedFileReaderFinishFailure {
    pub(crate) fn cancel(self) {
        if let Some(reader) = self.reader {
            reader.cancel();
        }
    }
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct PublicationLockKey {
    directory: DirectoryIdentity,
    name: PortablePathKey,
}

#[derive(Default)]
struct PublicationLockState {
    readers: usize,
    writer: bool,
}

#[derive(Default)]
struct PublicationLock {
    state: Mutex<PublicationLockState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PublicationLockMode {
    Shared,
    Exclusive,
}

pub(crate) struct ManagedPersistentFile {
    directory: ManagedDir,
    name: String,
    identity: ManagedFileIdentity,
    lock: Arc<PublicationLock>,
    held: Mutex<Option<PublicationLockMode>>,
}

impl std::fmt::Debug for ManagedPersistentFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedPersistentFile")
            .finish_non_exhaustive()
    }
}

#[derive(Debug)]
pub(crate) enum ManagedCreateOnlyWriteFailure {
    BeforePromotion(LoaderError),
    PromotionAttempted {
        final_guard: Option<ManagedFileGuard>,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedCreateOnlyWriteFault {
    TempCreated,
    BytesWritten,
    FileSynced,
    TempVerified,
    Promotion,
    DirectorySynced,
    FinalVerified,
    Revalidated,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedEmptyChildRemoval {
    Removed,
    IdentityMismatchRestored,
    IdentityMismatchParked,
}

#[derive(Debug)]
pub(crate) enum ManagedDirectoryMoveFailure {
    BeforeMove,
    MoveAttempted,
}

impl std::fmt::Debug for ManagedDir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedDir")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl ManagedRoot {
    fn transition(self: &Arc<Self>) -> ManagedEffectTransition<'_> {
        ManagedEffectTransition {
            root: self,
            _guard: self
                .effect_transition
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        }
    }

    fn require_transition(&self, transition: &ManagedEffectTransition<'_>) {
        assert!(std::ptr::eq(self, Arc::as_ptr(transition.root)));
    }

    fn validate_requested_binding(&self, path: &Path) -> Result<(), LoaderError> {
        let session = self._session.as_ref().ok_or_else(|| {
            LoaderError::Verify(
                "cached managed root does not retain its root session".to_string(),
            )
        })?;
        let ManagedRootSession::Direct(session) = session else {
            return Err(LoaderError::Verify(
                "admitted managed root cannot be rebound through a raw path".to_string(),
            ));
        };
        let observed = session.admit_absolute_directory(path)?;
        if observed.identity()? != self.anchor.identity()? {
            return Err(LoaderError::Verify(
                "managed root path changed binding".to_string(),
            ));
        }
        Ok(())
    }

    fn require_settled(self: &Arc<Self>) -> Result<(), LoaderError> {
        let transition = self.transition();
        self.require_settled_locked(&transition)
    }

    fn validate_retained_authority(&self) -> Result<(), LoaderError> {
        self._session
            .as_ref()
            .ok_or_else(|| {
                LoaderError::Verify(
                    "managed root does not retain its root session".to_string(),
                )
            })?
            .validate_retained_authority()
            .map_err(LoaderError::Io)
    }

    fn require_settled_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
    ) -> Result<(), LoaderError> {
        self.require_transition(transition);
        if !self
            .continuations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .receipts
            .is_empty()
        {
            return Err(unsettled(
                "managed filesystem terminal decision remains unclaimed",
            ));
        }
        self.effects.require_settled().map_err(LoaderError::Io)
    }

    fn retain_continuation_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        continuation: ManagedEffectContinuation,
    ) {
        self.require_transition(transition);
        let mut continuations = self
            .continuations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if continuations.receipts.len() >= MAX_MANAGED_EFFECT_CONTINUATIONS {
            drop(continuations);
            fail_stop_linear(continuation);
        }
        let id = continuations.next_id;
        let Some(next_id) = continuations.next_id.checked_add(1) else {
            drop(continuations);
            fail_stop_linear(continuation);
        };
        if continuations.receipts.contains_key(&id) {
            drop(continuations);
            fail_stop_linear(continuation);
        }
        continuations.next_id = next_id;
        continuations.receipts.insert(id, continuation);
    }

    fn settle(self: &Arc<Self>) -> Result<(), LoaderError> {
        let transition = self.transition();
        self.settle_locked(&transition)
    }

    fn settle_locked(
        self: &Arc<Self>,
        transition: &ManagedEffectTransition<'_>,
    ) -> Result<(), LoaderError> {
        self.require_transition(transition);
        let _initial_settlement = self.effects.settle();
        let pending = {
            let mut continuations = self
                .continuations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            std::mem::take(&mut continuations.receipts)
        };
        let mut unresolved = BTreeMap::new();
        for (id, continuation) in pending {
            match continuation.claim(transition) {
                Some(continuation) => {
                    unresolved.insert(id, continuation);
                }
                None => {}
            }
        }
        {
            let mut continuations = self
                .continuations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (id, continuation) in unresolved {
                if continuations.receipts.contains_key(&id) {
                    drop(continuations);
                    fail_stop_linear(continuation);
                }
                continuations.receipts.insert(id, continuation);
            }
        }
        let final_settlement = self.effects.settle();
        let final_truth = self.require_settled_locked(transition);
        final_settlement.map_err(LoaderError::Io)?;
        final_truth
    }

    fn intern_file(
        &self,
        candidate: FileCapability,
        operation_pin: Option<Arc<ManagedOperationPin>>,
    ) -> ManagedFileIdentity {
        let mut identities = self
            .file_identities
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        identities.retain(|identity| identity.strong_count() > 0);
        for retained in identities.iter().filter_map(Weak::upgrade) {
            let matches = retained
                .capability
                .lock()
                .ok()
                .and_then(|capability| capability.as_ref().map(|value| value.same_file(&candidate)))
                .transpose()
                .ok()
                .flatten()
                .unwrap_or(false);
            if matches {
                return ManagedFileIdentity {
                    proof: retained,
                    _operation_pin: operation_pin,
                };
            }
        }
        let proof = Arc::new(ManagedFileProof {
            capability: Mutex::new(Some(candidate)),
        });
        identities.push(Arc::downgrade(&proof));
        ManagedFileIdentity {
            proof,
            _operation_pin: operation_pin,
        }
    }

    fn publication_lock(
        &self,
        directory: DirectoryIdentity,
        name: &str,
    ) -> Result<Arc<PublicationLock>, LoaderError> {
        let key = PublicationLockKey {
            directory,
            name: portable_key(name)?,
        };
        let mut locks = self
            .publication_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        locks.retain(|_, lock| lock.strong_count() > 0);
        if let Some(lock) = locks.get(&key).and_then(Weak::upgrade) {
            return Ok(lock);
        }
        let lock = Arc::new(PublicationLock::default());
        locks.insert(key, Arc::downgrade(&lock));
        Ok(lock)
    }

    fn retain_linear_locked<T, R>(
        &self,
        transition: &ManagedEffectTransition<'_>,
        carrier: T,
        retain: impl Fn(&EffectOwner, T) -> Result<R, axial_fs::EffectOwnerRetentionError<T>>,
    ) -> R {
        self.require_transition(transition);
        let (error, carrier) = match retain(&self.effects, carrier) {
            Ok(retained) => return retained,
            Err(failure) => failure.into_parts(),
        };
        if error.kind() != io::ErrorKind::WouldBlock {
            fail_stop_linear(carrier);
        }
        let _ = self.effects.settle();
        match retain(&self.effects, carrier) {
            Ok(retained) => retained,
            Err(failure) => {
                let (_, carrier) = failure.into_parts();
                fail_stop_linear(carrier)
            }
        }
    }
}

fn fail_stop_linear<T>(carrier: T) -> ! {
    let _carrier = carrier;
    std::process::abort()
}

impl ManagedEffectContinuation {
    fn claim(self, transition: &ManagedEffectTransition<'_>) -> Option<Self> {
        match self {
            Self::FilePromotion(receipt) => match receipt.claim() {
                FilePromotionReceiptOutcome::Pending(receipt) => {
                    Some(Self::FilePromotion(receipt))
                }
                FilePromotionReceiptOutcome::Applied(file) => {
                    drop(file);
                    None
                }
                FilePromotionReceiptOutcome::NoEffect(staged) => {
                    retain_stage_discard_locked(transition, staged.discard());
                    None
                }
            },
            Self::FileReplace(receipt) => match receipt.claim() {
                FileReplaceReceiptOutcome::Pending(receipt) => Some(Self::FileReplace(receipt)),
                FileReplaceReceiptOutcome::Replaced { current, displaced } => {
                    drop(current);
                    if let Some(displaced) = displaced {
                        retain_parked_file_removal_locked(transition, displaced);
                    }
                    None
                }
                FileReplaceReceiptOutcome::NoEffect {
                    staged,
                    destination,
                } => {
                    drop(destination);
                    retain_stage_discard_locked(transition, staged.discard());
                    None
                }
            },
            Self::FileMove { receipt, identity } => match receipt.claim() {
                FileMoveReceiptOutcome::Pending(receipt) => {
                    Some(Self::FileMove { receipt, identity })
                }
                FileMoveReceiptOutcome::Applied(file)
                | FileMoveReceiptOutcome::NoEffect(file) => {
                    replace_file_proof_capability(&identity, file);
                    None
                }
            },
            Self::DirectoryMove(receipt) => match receipt.claim() {
                DirectoryMoveReceiptOutcome::Pending(receipt) => {
                    Some(Self::DirectoryMove(receipt))
                }
                DirectoryMoveReceiptOutcome::Applied(directory)
                | DirectoryMoveReceiptOutcome::NoEffect(directory) => {
                    drop(directory);
                    None
                }
            },
            Self::TreeDirectoryMove {
                receipt,
                parent,
                stage_name,
                stage,
            } => match receipt.claim() {
                DirectoryMoveReceiptOutcome::Pending(receipt) => {
                    Some(Self::TreeDirectoryMove {
                        receipt,
                        parent,
                        stage_name,
                        stage,
                    })
                }
                DirectoryMoveReceiptOutcome::Applied(directory) => {
                    drop((directory, parent, stage_name, stage));
                    None
                }
                DirectoryMoveReceiptOutcome::NoEffect(directory) => {
                    drop(directory);
                    retain_tree_cleanup(transition, parent, stage_name, stage)
                }
            },
            Self::TreeCleanup {
                parent,
                stage_name,
                stage,
            } => retain_tree_cleanup(transition, parent, stage_name, stage),
        }
    }
}

fn retain_tree_cleanup(
    transition: &ManagedEffectTransition<'_>,
    parent: ManagedDirDescriptor,
    stage_name: PortableFileName,
    stage: ManagedDirDescriptor,
) -> Option<ManagedEffectContinuation> {
    let exact_descriptor = !stage.is_root
        && stage.path == parent.path.join(stage_name.as_str())
        && parent.directory.identity().ok() == Some(parent.identity);
    if !exact_descriptor {
        return Some(ManagedEffectContinuation::TreeCleanup {
            parent,
            stage_name,
            stage,
        });
    }
    let Ok(stage_leaf) = leaf(stage_name.as_str()) else {
        return Some(ManagedEffectContinuation::TreeCleanup {
            parent,
            stage_name,
            stage,
        });
    };
    let opened = match parent.directory.open_directory(&stage_leaf) {
        Ok(opened) => opened,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return None,
        Err(_) => {
            return Some(ManagedEffectContinuation::TreeCleanup {
                parent,
                stage_name,
                stage,
            });
        }
    };
    let Ok(opened_identity) = opened.identity() else {
        return Some(ManagedEffectContinuation::TreeCleanup {
            parent,
            stage_name,
            stage,
        });
    };
    if opened_identity != stage.identity {
        return None;
    }
    let parent_directory = parent.restore(transition.root);
    let stage_directory = stage.restore_with(opened, transition.root);
    if stage_directory
        .clear_contents_locked(transition, 0)
        .and_then(|()| parent_directory.remove_empty_child_locked(transition, &stage_directory))
        .is_err()
    {
        Some(ManagedEffectContinuation::TreeCleanup {
            parent,
            stage_name,
            stage,
        })
    } else {
        None
    }
}

fn retain_stage_discard_locked(
    transition: &ManagedEffectTransition<'_>,
    outcome: StageDiscardOutcome,
) {
    if let StageDiscardOutcome::AppliedUnverified(obligation) = outcome {
        transition.root.retain_linear_locked(
            transition,
            obligation,
            EffectOwner::retain_stage_discard,
        );
    }
}

fn retain_parked_file_removal_locked(
    transition: &ManagedEffectTransition<'_>,
    parked: ParkedFile,
) {
    match parked.remove() {
        FileRemovalOutcome::Removed => {}
        FileRemovalOutcome::NoEffect { parked, .. } => {
            transition.root.retain_linear_locked(
                transition,
                parked,
                EffectOwner::retain_parked_file_removal,
            );
        }
        FileRemovalOutcome::AppliedUnverified(obligation) => {
            transition.root.retain_linear_locked(
                transition,
                obligation,
                EffectOwner::retain_file_removal,
            );
        }
    }
}

fn retain_parked_directory_removal_locked(
    transition: &ManagedEffectTransition<'_>,
    parked: ParkedDirectory,
) {
    match parked.remove_empty() {
        DirectoryRemovalOutcome::Removed => {}
        DirectoryRemovalOutcome::NoEffect { parked, .. } => {
            transition.root.retain_linear_locked(
                transition,
                parked,
                EffectOwner::retain_parked_directory_removal,
            );
        }
        DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
            transition.root.retain_linear_locked(
                transition,
                obligation,
                EffectOwner::retain_directory_removal,
            );
        }
    }
}

impl ManagedDirDescriptor {
    fn capture(directory: &ManagedDir) -> Self {
        Self {
            directory: directory.inner.directory.clone(),
            identity: directory.inner.identity,
            path: directory.inner.path.clone(),
            is_root: directory.inner.is_root,
        }
    }

    fn restore(&self, root: &Arc<ManagedRoot>) -> ManagedDir {
        self.restore_with(self.directory.clone(), root)
    }

    fn restore_with(&self, directory: Directory, root: &Arc<ManagedRoot>) -> ManagedDir {
        ManagedDir::from_directory_inner(
            directory,
            self.identity,
            root.clone(),
            None,
            self.path.clone(),
            self.is_root,
        )
    }
}

impl ManagedFileIdentity {
    fn replace_capability(&self, file: FileCapability) {
        replace_file_proof_capability(&self.proof, file);
    }

    fn pinless_proof(&self) -> Arc<ManagedFileProof> {
        Arc::clone(&self.proof)
    }

    fn mark_unsettled(&self) {
        *self
            .proof
            .capability
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    fn matches(&self, file: &FileCapability) -> io::Result<bool> {
        verify_operation_admission(&self._operation_pin)?;
        let capability = self
            .proof
            .capability
            .lock()
            .map_err(|_| io::Error::other("managed file identity lock was poisoned"))?;
        let matches = capability
            .as_ref()
            .ok_or_else(|| io::Error::new(io::ErrorKind::WouldBlock, "managed file move is unsettled"))?
            .same_file(file)?;
        drop(capability);
        verify_operation_admission(&self._operation_pin)?;
        Ok(matches)
    }

    fn with_capability<T>(
        &self,
        operation: impl FnOnce(&FileCapability) -> Result<T, LoaderError>,
    ) -> Result<T, LoaderError> {
        verify_operation_admission(&self._operation_pin)?;
        let capability = self
            .proof
            .capability
            .lock()
            .map_err(|_| LoaderError::Verify("managed file identity lock was poisoned".to_string()))?;
        let value = operation(capability.as_ref().ok_or_else(|| {
            unsettled("managed file move remains unsettled")
        })?)?;
        drop(capability);
        verify_operation_admission(&self._operation_pin)?;
        Ok(value)
    }
}

fn replace_file_proof_capability(proof: &ManagedFileProof, file: FileCapability) {
    *proof
        .capability
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(file);
}

fn has_portably_exact_name(
    entries: Vec<DirectoryEntry>,
    expected: &str,
) -> Result<bool, LoaderError> {
    let expected_key = portable_key(expected)?;
    let mut exact = false;
    for entry in entries {
        let name = entry.utf8_name().ok_or_else(|| {
            LoaderError::Verify("managed directory contains a non-UTF-8 name".to_string())
        })?;
        let key = portable_key(name)?;
        if key == expected_key {
            if exact || name != expected {
                return Err(LoaderError::Verify(
                    "managed directory contains a portable path alias".to_string(),
                ));
            }
            exact = true;
        }
    }
    Ok(exact)
}

impl ManagedDir {
    fn with_operation_pin(&self, pin: Arc<ManagedOperationPin>) -> Self {
        Self::from_directory_inner(
            self.inner.directory.clone(),
            self.inner.identity,
            Arc::clone(&self.inner.root),
            Some(pin),
            self.inner.path.clone(),
            self.inner.is_root,
        )
    }

    pub(crate) fn from_directory(
        directory: Directory,
        effects: EffectOwner,
    ) -> Result<Self, LoaderError> {
        Self::from_directory_with_session(directory, effects, None)
    }

    fn from_directory_with_session(
        directory: Directory,
        effects: EffectOwner,
        session: Option<ManagedRootSession>,
    ) -> Result<Self, LoaderError> {
        let identity = match directory.identity() {
            Ok(identity) => identity,
            Err(error) => {
                drop(effects);
                drop(directory);
                drop(session);
                return Err(error.into());
            }
        };
        if effects.anchor_identity() != identity {
            drop(effects);
            drop(directory);
            drop(session);
            return Err(LoaderError::Verify(
                "managed effect owner is not anchored at the admitted root".to_string(),
            ));
        }
        let root = Arc::new(ManagedRoot {
            anchor: directory.clone(),
            effects,
            effect_transition: Mutex::new(()),
            continuations: Mutex::new(ManagedEffectContinuations {
                next_id: 1,
                receipts: BTreeMap::new(),
            }),
            file_identities: Mutex::new(Vec::new()),
            publication_locks: Mutex::new(HashMap::new()),
            publication_mutex: Arc::new(tokio::sync::Mutex::new(())),
            install_flights: Mutex::new(HashMap::new()),
            // RootSession remains last so owner/capability fields are released first.
            _session: session,
        });
        Ok(Self::from_directory_inner(
            directory,
            identity,
            root,
            None,
            PathBuf::new(),
            true,
        ))
    }

    pub(crate) fn open_root(path: &Path) -> Result<Self, LoaderError> {
        let requested_key = absolute_root_key(path)?;
        let mut registry = roots()
            .lock()
            .map_err(|_| LoaderError::Verify("managed root registry was poisoned".to_string()))?;
        registry.retain(|_, root| root.strong_count() > 0);
        if let Some(root) = registry.get(&requested_key).and_then(Weak::upgrade) {
            drop(registry);
            root.validate_requested_binding(&requested_key)?;
            root.settle()?;
            root.validate_requested_binding(&requested_key)?;
            let directory = root.anchor.clone();
            let identity = directory.identity()?;
            return Ok(Self::from_directory_inner(
                directory,
                identity,
                root,
                None,
                requested_key,
                true,
            ));
        }

        let session = acquire_root_session(path)?;
        let directory = session.root()?;
        let identity = directory.identity()?;
        let effects = directory.create_effect_owner()?;
        let root = Arc::new(ManagedRoot {
            anchor: directory.clone(),
            effects,
            effect_transition: Mutex::new(()),
            continuations: Mutex::new(ManagedEffectContinuations {
                next_id: 1,
                receipts: BTreeMap::new(),
            }),
            file_identities: Mutex::new(Vec::new()),
            publication_locks: Mutex::new(HashMap::new()),
            publication_mutex: Arc::new(tokio::sync::Mutex::new(())),
            install_flights: Mutex::new(HashMap::new()),
            // RootSession remains last so owner/capability fields are released first.
            _session: Some(ManagedRootSession::Direct(session)),
        });
        registry.insert(requested_key.clone(), Arc::downgrade(&root));
        Ok(Self::from_directory_inner(
            directory,
            identity,
            root,
            None,
            requested_key,
            true,
        ))
    }

    fn from_directory_inner(
        directory: Directory,
        identity: DirectoryIdentity,
        root: Arc<ManagedRoot>,
        operation_pin: Option<Arc<ManagedOperationPin>>,
        path: PathBuf,
        is_root: bool,
    ) -> Self {
        Self {
            inner: Arc::new(ManagedDirInner {
                directory,
                identity,
                root,
                operation_pin,
                path,
                is_root,
            }),
        }
    }

    fn child_from_directory(&self, name: &str, directory: Directory) -> Result<Self, LoaderError> {
        verify_operation_admission(&self.inner.operation_pin)?;
        let identity = directory.identity()?;
        let child = Self::from_directory_inner(
            directory,
            identity,
            self.inner.root.clone(),
            self.inner.operation_pin.clone(),
            self.inner.path.join(name),
            false,
        );
        child.revalidate()?;
        Ok(child)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.inner.path
    }

    pub(crate) fn identity(&self) -> Result<ManagedDirectoryIdentity, LoaderError> {
        self.revalidate()?;
        Ok(ManagedDirectoryIdentity(self.inner.identity))
    }

    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        verify_operation_admission(&self.inner.operation_pin)?;
        self.inner.root.require_settled()?;
        self.revalidate_locked_root()?;
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(())
    }

    pub(crate) fn settle(&self) -> Result<(), LoaderError> {
        verify_operation_admission(&self.inner.operation_pin)?;
        self.inner.root.settle()?;
        self.revalidate_locked_root()?;
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(())
    }

    pub(crate) fn shares_root(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner.root, &other.inner.root)
    }

    pub(crate) fn publication_mutex(
        &self,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, LoaderError> {
        self.revalidate()?;
        Ok(Arc::clone(&self.inner.root.publication_mutex))
    }

    pub(crate) fn install_flight(
        &self,
        version_id: PortablePathKey,
        max_live: usize,
    ) -> Result<Arc<tokio::sync::Mutex<()>>, LoaderError> {
        let mut flights = self
            .inner
            .root
            .install_flights
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        flights.retain(|_, flight| flight.strong_count() > 0);
        if let Some(flight) = flights.get(&version_id).and_then(Weak::upgrade) {
            return Ok(flight);
        }
        if max_live == 0 || flights.len() >= max_live {
            return Err(LoaderError::InstallExecutionFailed(
                "loader install flight capacity is exhausted".to_string(),
            ));
        }
        let flight = Arc::new(tokio::sync::Mutex::new(()));
        flights.insert(version_id, Arc::downgrade(&flight));
        Ok(flight)
    }

    fn revalidate_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
    ) -> Result<(), LoaderError> {
        self.inner.root.require_transition(transition);
        verify_operation_admission(&self.inner.operation_pin)?;
        self.revalidate_locked_root()?;
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(())
    }

    fn revalidate_locked_root(&self) -> Result<(), LoaderError> {
        if self.inner.directory.identity()? != self.inner.identity {
            return Err(LoaderError::Verify(
                "managed directory capability changed identity".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), LoaderError> {
        self.revalidate()?;
        self.inner.directory.sync()?;
        self.revalidate()
    }

    pub(crate) fn open_child(&self, name: &str) -> Result<Self, LoaderError> {
        let name_leaf = leaf(name)?;
        self.revalidate()?;
        let directory = self.inner.directory.open_directory(&name_leaf)?;
        self.child_from_directory(name, directory)
    }

    pub(crate) fn open_observed_child(&self, entry: &DirectoryEntry) -> Result<Self, LoaderError> {
        self.revalidate()?;
        let name = entry.utf8_name().ok_or_else(|| {
            LoaderError::Verify("managed directory contains a non-UTF-8 name".to_string())
        })?;
        PortableFileName::new_exact(name).map_err(|_| {
            LoaderError::Verify("managed directory contains a non-portable name".to_string())
        })?;
        let directory = self.inner.directory.open_observed_directory(entry)?;
        self.child_from_directory(name, directory)
    }

    pub(crate) fn open_or_create_child(&self, name: &str) -> Result<Self, LoaderError> {
        let name_leaf = leaf(name)?;
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.revalidate_locked(&transition)?;
        if self.has_portably_exact_child_name_locked(&transition, name)? {
            let directory = self.inner.directory.open_directory(&name_leaf)?;
            return self.child_from_directory(name, directory);
        }
        match self.inner.directory.create_directory(&name_leaf) {
            DirectoryCreateOutcome::Created(directory) => {
                self.inner.directory.sync()?;
                self.child_from_directory(name, directory)
            }
            DirectoryCreateOutcome::NoEffect(error)
                if error.kind() == io::ErrorKind::AlreadyExists =>
            {
                if !self.has_portably_exact_child_name_locked(&transition, name)? {
                    return Err(error.into());
                }
                let directory = self.inner.directory.open_directory(&name_leaf)?;
                self.child_from_directory(name, directory)
            }
            DirectoryCreateOutcome::NoEffect(error) => Err(error.into()),
            DirectoryCreateOutcome::CreatedUnclassified {
                error,
                preservation,
            } => {
                if let Err(preservation) = preservation.acknowledge_preserved() {
                    root.retain_linear_locked(
                        &transition,
                        preservation,
                        EffectOwner::retain_directory_create_preservation,
                    );
                    root.settle_locked(&transition)?;
                }
                Err(LoaderError::Io(error))
            }
            DirectoryCreateOutcome::AppliedUnverified(obligation) => {
                root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_directory_create_completion,
                );
                root.settle_locked(&transition)?;
                Err(unsettled("managed child creation remains unsettled"))
            }
        }
    }

    pub(crate) fn create_child_new(&self, name: &str) -> Result<Self, LoaderError> {
        let name_leaf = leaf(name)?;
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.revalidate_locked(&transition)?;
        if self.has_portably_exact_child_name_locked(&transition, name)? {
            return Err(LoaderError::Io(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "managed child already exists",
            )));
        }
        match self.inner.directory.create_directory(&name_leaf) {
            DirectoryCreateOutcome::Created(directory) => {
                self.inner.directory.sync()?;
                self.child_from_directory(name, directory)
            }
            DirectoryCreateOutcome::NoEffect(error) => Err(error.into()),
            DirectoryCreateOutcome::CreatedUnclassified {
                error,
                preservation,
            } => {
                if let Err(preservation) = preservation.acknowledge_preserved() {
                    root.retain_linear_locked(
                        &transition,
                        preservation,
                        EffectOwner::retain_directory_create_preservation,
                    );
                    root.settle_locked(&transition)?;
                }
                Err(LoaderError::Io(error))
            }
            DirectoryCreateOutcome::AppliedUnverified(obligation) => {
                root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_directory_create_completion,
                );
                root.settle_locked(&transition)?;
                Err(unsettled("managed child creation remains unsettled"))
            }
        }
    }

    fn listing(&self, limit: usize) -> Result<Vec<DirectoryEntry>, LoaderError> {
        self.revalidate()?;
        self.listing_after_revalidation(limit)
    }

    fn listing_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        limit: usize,
    ) -> Result<Vec<DirectoryEntry>, LoaderError> {
        self.revalidate_locked(transition)?;
        self.listing_after_revalidation(limit)
    }

    fn listing_after_revalidation(
        &self,
        limit: usize,
    ) -> Result<Vec<DirectoryEntry>, LoaderError> {
        if limit == 0 || limit > MAX_MANAGED_TREE_OPERATION_ENTRIES {
            return Err(LoaderError::Verify(
                "managed directory listing bound is invalid".to_string(),
            ));
        }
        let requested = limit.checked_add(usize::from(self.inner.is_root)).ok_or_else(|| {
            LoaderError::Verify("managed directory listing bound overflowed".to_string())
        })?;
        let listing = self.inner.directory.entries(requested)?;
        if listing.state() != DirectoryListingState::Complete {
            return Err(LoaderError::Verify(
                "managed directory exceeds its listing bound".to_string(),
            ));
        }
        let entries = listing
            .entries()
            .iter()
            .filter(|entry| {
                !(self.inner.is_root && entry.name().to_str() == Some(ROOT_LEASE_NAME))
            })
            .cloned()
            .collect::<Vec<_>>();
        if entries.len() > limit {
            return Err(LoaderError::Verify(
                "managed directory exceeds its listing bound".to_string(),
            ));
        }
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(entries)
    }

    pub(crate) fn entries_bounded(&self, limit: usize) -> Result<Vec<OsString>, LoaderError> {
        self.listing(limit)
            .map(|entries| entries.into_iter().map(|entry| entry.name().to_owned()).collect())
    }

    pub(crate) fn guarded_entries_bounded(
        &self,
        limit: usize,
    ) -> Result<Vec<DirectoryEntry>, LoaderError> {
        self.listing(limit)
    }

    pub(crate) fn passive_revision(&self) -> Result<axial_fs::DirectoryRevision, LoaderError> {
        self.revalidate()?;
        let revision = self.inner.directory.revision()?;
        self.revalidate()?;
        Ok(revision)
    }

    pub(crate) fn validate_passive_revision(
        &self,
        revision: &axial_fs::DirectoryRevision,
    ) -> Result<(), LoaderError> {
        self.revalidate()?;
        self.inner.directory.validate_revision(revision)?;
        self.revalidate()
    }

    pub(crate) fn validate_passive_file_revision(
        &self,
        name: &str,
        revision: &ManagedPassiveFileRevision,
    ) -> Result<(), LoaderError> {
        let name = leaf(name)?;
        self.revalidate()?;
        let file = self.inner.directory.open_file(&name)?;
        file.validate_revision_observation(&revision.0)?;
        self.revalidate()
    }

    pub(crate) fn entries(&self) -> Result<Vec<OsString>, LoaderError> {
        self.entries_bounded(MAX_MANAGED_DIRECTORY_ENTRIES)
    }

    pub(crate) fn has_portably_exact_child_name(
        &self,
        expected: &str,
    ) -> Result<bool, LoaderError> {
        let entries = self.listing(MAX_MANAGED_DIRECTORY_ENTRIES)?;
        has_portably_exact_name(entries, expected)
    }

    fn has_portably_exact_child_name_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        expected: &str,
    ) -> Result<bool, LoaderError> {
        let entries = self.listing_locked(transition, MAX_MANAGED_DIRECTORY_ENTRIES)?;
        has_portably_exact_name(entries, expected)
    }

    pub(crate) fn open_or_create_relative_parent(
        &self,
        relative: &PortableRelativePath,
    ) -> Result<(Self, String), LoaderError> {
        let mut segments = relative.as_str().split('/').peekable();
        let mut directory = self.clone();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                return Ok((directory, segment.to_string()));
            }
            directory = directory.open_or_create_child(segment)?;
        }
        Err(LoaderError::Verify(
            "managed relative path has no file name".to_string(),
        ))
    }

    pub(crate) fn inspect_regular_file(
        &self,
        name: &str,
    ) -> Result<Option<ManagedFileGuard>, LoaderError> {
        self.revalidate()?;
        self.inspect_regular_file_after_revalidation(name)
    }

    fn inspect_regular_file_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        name: &str,
    ) -> Result<Option<ManagedFileGuard>, LoaderError> {
        self.revalidate_locked(transition)?;
        self.inspect_regular_file_after_revalidation(name)
    }

    fn inspect_regular_file_after_revalidation(
        &self,
        name: &str,
    ) -> Result<Option<ManagedFileGuard>, LoaderError> {
        verify_operation_admission(&self.inner.operation_pin)?;
        let name_leaf = leaf(name)?;
        let file = match self.inner.directory.open_file(&name_leaf) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                verify_operation_admission(&self.inner.operation_pin)?;
                return Ok(None);
            }
            Err(error) => return Err(error.into()),
        };
        let revision = file.revision()?;
        let size = revision.size();
        let identity = self
            .inner
            .root
            .intern_file(file, self.inner.operation_pin.clone());
        let guard = ManagedFileGuard {
            directory: self.inner.directory.clone(),
            name: name_leaf,
            identity,
            revision,
            size,
            _operation_pin: self.inner.operation_pin.clone(),
        };
        if !self.file_guard_matches_after_revalidation(name, &guard)? {
            return Err(LoaderError::Verify(
                "managed file changed during admission".to_string(),
            ));
        }
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(Some(guard))
    }

    pub(crate) fn file_guard_matches(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<bool, LoaderError> {
        self.revalidate()?;
        self.file_guard_matches_after_revalidation(name, guard)
    }

    fn file_guard_matches_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<bool, LoaderError> {
        self.revalidate_locked(transition)?;
        self.file_guard_matches_after_revalidation(name, guard)
    }

    fn file_guard_matches_after_revalidation(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<bool, LoaderError> {
        verify_operation_admission(&self.inner.operation_pin)?;
        let name_leaf = leaf(name)?;
        let file = match self.inner.directory.open_file(&name_leaf) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                verify_operation_admission(&self.inner.operation_pin)?;
                return Ok(false);
            }
            Err(error) => return Err(error.into()),
        };
        if !guard.identity.matches(&file)? {
            return Ok(false);
        }
        let matches = file.validate_revision(&guard.revision).is_ok()
            && file.revision()?.size() == guard.size;
        verify_operation_admission(&self.inner.operation_pin)?;
        Ok(matches)
    }

    pub(crate) fn read_guarded_file_bounded(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<Vec<u8>, LoaderError> {
        if guard.size > max_size || guard.size > MAX_MANAGED_READ_BYTES {
            return Err(LoaderError::Verify(
                "managed guarded read exceeds its admitted bound".to_string(),
            ));
        }
        if !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded read source changed".to_string(),
            ));
        }
        let bytes = guard.identity.with_capability(|file| {
            file.validate_revision(&guard.revision)?;
            let bytes = file.read_bounded(max_size)?;
            file.validate_revision(&guard.revision)?;
            Ok(bytes)
        })?;
        if bytes.len() as u64 != guard.size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded read source changed during reading".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn sha1_guarded_file_bytes_with_check(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
        mut check: impl FnMut() -> Result<(), LoaderError>,
    ) -> Result<[u8; 20], LoaderError> {
        check()?;
        if guard.size > max_size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source is invalid or exceeds its bound".to_string(),
            ));
        }
        let digest = guard.identity.with_capability(|file| {
            file.validate_revision(&guard.revision)?;
            let mut reader = file.reader(max_size)?;
            let mut observed = 0_u64;
            let mut hasher = Sha1::new();
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                check()?;
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                observed = observed.checked_add(read as u64).ok_or_else(|| {
                    LoaderError::Verify("managed guarded hash size overflowed".to_string())
                })?;
                hasher.update(&chunk[..read]);
            }
            reader.finish()?;
            if observed != guard.size {
                return Err(LoaderError::Verify(
                    "managed guarded hash source changed size".to_string(),
                ));
            }
            Ok(<[u8; 20]>::from(hasher.finalize()))
        })?;
        check()?;
        if !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source changed during hashing".to_string(),
            ));
        }
        #[cfg(test)]
        record_sha1_full_read(&self.inner.path, guard.size, digest);
        Ok(digest)
    }

    pub(crate) fn sha1_guarded_file_bytes(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<[u8; 20], LoaderError> {
        self.sha1_guarded_file_bytes_with_check(name, guard, max_size, || Ok(()))
    }

    pub(crate) fn sha1_guarded_file(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<String, LoaderError> {
        Ok(hex_lower(&self.sha1_guarded_file_bytes(name, guard, max_size)?))
    }

    pub(crate) fn sha512_guarded_file(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<String, LoaderError> {
        if guard.size > max_size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source is invalid or exceeds its bound".to_string(),
            ));
        }
        let digest = guard.identity.with_capability(|file| {
            file.validate_revision(&guard.revision)?;
            let mut reader = file.reader(max_size)?;
            let mut hasher = Sha512::new();
            let mut observed = 0_u64;
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                let read = reader.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                observed = observed.checked_add(read as u64).ok_or_else(|| {
                    LoaderError::Verify("managed guarded hash size overflowed".to_string())
                })?;
                hasher.update(&chunk[..read]);
            }
            reader.finish()?;
            if observed != guard.size {
                return Err(LoaderError::Verify(
                    "managed guarded hash source changed size".to_string(),
                ));
            }
            Ok(<[u8; 64]>::from(hasher.finalize()))
        })?;
        if !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source changed during hashing".to_string(),
            ));
        }
        Ok(hex_lower(&digest))
    }

    fn create_stage(&self) -> Result<StagedFile, LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.revalidate_locked(&transition)?;
        match self.inner.directory.create_stage() {
            FileCreateOutcome::Created(staged) => Ok(staged),
            FileCreateOutcome::NoEffect(error) => Err(error.into()),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_stage_create_cleanup,
                );
                root.settle_locked(&transition)?;
                Err(unsettled("managed stage creation remains unsettled"))
            }
        }
    }

    fn discard_stage(&self, stage: StagedFile) -> Result<(), LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        retain_stage_discard_locked(&transition, stage.discard());
        root.settle_locked(&transition)
    }

    fn discard_sealed_stage(&self, stage: SealedStagedFile) -> Result<(), LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        self.discard_sealed_stage_locked(&transition, stage)
    }

    fn discard_sealed_stage_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        stage: SealedStagedFile,
    ) -> Result<(), LoaderError> {
        retain_stage_discard_locked(transition, stage.discard());
        self.inner.root.settle_locked(transition)
    }

    fn seal_stage(&self, stage: StagedFile) -> Result<SealedStagedFile, LoaderError> {
        match stage.seal() {
            Ok(sealed) => Ok(sealed),
            Err(failure) => {
                let error = io::Error::new(failure.error().kind(), failure.error().to_string());
                self.discard_stage(failure.into_staged())?;
                Err(error.into())
            }
        }
    }

    fn guard_from_file(
        &self,
        name: LeafName,
        file: FileCapability,
    ) -> Result<ManagedFileGuard, LoaderError> {
        let revision = file.revision()?;
        let size = revision.size();
        let identity = self
            .inner
            .root
            .intern_file(file, self.inner.operation_pin.clone());
        Ok(ManagedFileGuard {
            directory: self.inner.directory.clone(),
            name,
            identity,
            revision,
            size,
            _operation_pin: self.inner.operation_pin.clone(),
        })
    }

    fn promote_create_new(
        &self,
        name: LeafName,
        sealed: SealedStagedFile,
    ) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition).map_err(|_| {
            ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard: None }
        })?;
        match sealed.promote_no_replace(
            &self.inner.directory,
            &self.inner.directory,
            &name,
        ) {
            FilePromotionOutcome::Applied(file) => self
                .guard_from_file(name, file)
                .map_err(|_| ManagedCreateOnlyWriteFailure::PromotionAttempted {
                    final_guard: None,
                }),
            FilePromotionOutcome::NoEffect { error: _, staged } => {
                if self
                    .discard_sealed_stage_locked(&transition, staged)
                    .is_err()
                {
                    return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                        final_guard: None,
                    });
                }
                Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                    final_guard: None,
                })
            }
            FilePromotionOutcome::AppliedUnverified(obligation) => {
                let receipt = root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_file_promotion,
                );
                let _settlement = root.effects.settle();
                match receipt.claim() {
                    FilePromotionReceiptOutcome::Applied(file) => {
                        let guard = self.guard_from_file(name, file).map_err(|_| {
                            ManagedCreateOnlyWriteFailure::PromotionAttempted {
                                final_guard: None,
                            }
                        })?;
                        if root.settle_locked(&transition).is_err() {
                            return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                                final_guard: Some(guard),
                            });
                        }
                        Ok(guard)
                    }
                    FilePromotionReceiptOutcome::NoEffect(staged) => {
                        if self
                            .discard_sealed_stage_locked(&transition, staged)
                            .is_err()
                        {
                            return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                                final_guard: None,
                            });
                        }
                        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                            final_guard: None,
                        })
                    }
                    FilePromotionReceiptOutcome::Pending(receipt) => {
                        root.retain_continuation_locked(
                            &transition,
                            ManagedEffectContinuation::FilePromotion(receipt),
                        );
                        Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                            final_guard: None,
                        })
                    }
                }
            }
        }
    }

    pub(crate) fn write_new_exact_retained(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
        self.write_new_exact_retained_inner(
            name,
            bytes,
            #[cfg(test)]
            None,
        )
    }

    #[cfg(test)]
    pub(crate) fn write_new_exact_retained_with_fault(
        &self,
        name: &str,
        bytes: &[u8],
        fault: ManagedCreateOnlyWriteFault,
    ) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
        self.write_new_exact_retained_inner(name, bytes, Some(fault))
    }

    fn write_new_exact_retained_inner(
        &self,
        name: &str,
        bytes: &[u8],
        #[cfg(test)] fault: Option<ManagedCreateOnlyWriteFault>,
    ) -> Result<ManagedFileGuard, ManagedCreateOnlyWriteFailure> {
        let name_leaf = leaf(name).map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
        let size = u64::try_from(bytes.len()).map_err(|_| {
            ManagedCreateOnlyWriteFailure::BeforePromotion(LoaderError::Verify(
                "managed create-only write size overflowed".to_string(),
            ))
        })?;
        if size > MAX_MANAGED_READ_BYTES {
            return Err(ManagedCreateOnlyWriteFailure::BeforePromotion(
                LoaderError::Verify("managed create-only write exceeds its bound".to_string()),
            ));
        }
        let mut staged = self
            .create_stage()
            .map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
        #[cfg(test)]
        if fault == Some(ManagedCreateOnlyWriteFault::TempCreated) {
            self.discard_stage(staged)
                .map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
            return Err(ManagedCreateOnlyWriteFailure::BeforePromotion(
                injected_create_only_write_failure(),
            ));
        }
        if let Err(error) = staged.write_all(bytes) {
            self.discard_stage(staged)
                .map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
            return Err(ManagedCreateOnlyWriteFailure::BeforePromotion(error.into()));
        }
        #[cfg(test)]
        if matches!(
            fault,
            Some(ManagedCreateOnlyWriteFault::BytesWritten)
                | Some(ManagedCreateOnlyWriteFault::FileSynced)
                | Some(ManagedCreateOnlyWriteFault::TempVerified)
        ) {
            self.discard_stage(staged)
                .map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
            return Err(ManagedCreateOnlyWriteFailure::BeforePromotion(
                injected_create_only_write_failure(),
            ));
        }
        let sealed = self
            .seal_stage(staged)
            .map_err(ManagedCreateOnlyWriteFailure::BeforePromotion)?;
        let guard = self.promote_create_new(name_leaf, sealed)?;
        #[cfg(test)]
        if matches!(
            fault,
            Some(ManagedCreateOnlyWriteFault::Promotion)
                | Some(ManagedCreateOnlyWriteFault::DirectorySynced)
                | Some(ManagedCreateOnlyWriteFault::FinalVerified)
                | Some(ManagedCreateOnlyWriteFault::Revalidated)
        ) {
            return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                final_guard: Some(guard),
            });
        }
        let verified = self
            .read_guarded_file_bounded(name, &guard, size)
            .is_ok_and(|written| written == bytes);
        if self.sync().is_err() || !verified {
            return Err(ManagedCreateOnlyWriteFailure::PromotionAttempted {
                final_guard: Some(guard),
            });
        }
        Ok(guard)
    }

    pub(crate) fn write_new_exact_guarded(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<ManagedFileGuard, LoaderError> {
        match self.write_new_exact_retained(name, bytes) {
            Ok(guard) => Ok(guard),
            Err(ManagedCreateOnlyWriteFailure::BeforePromotion(error)) => Err(error),
            Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { .. }) => Err(unsettled(
                "managed create-only publication could not be classified",
            )),
        }
    }

    pub(crate) fn write_new_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        self.write_new_exact_guarded(name, bytes).map(drop)
    }

    fn replace_or_promote(
        &self,
        name: LeafName,
        sealed: SealedStagedFile,
    ) -> Result<ManagedFileGuard, LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        let destination = match self.inner.directory.open_file(&name) {
            Ok(file) => {
                let revision = file.revision()?;
                let bytes = file.read_bounded(MAX_MANAGED_READ_BYTES)?;
                file.validate_revision(&revision)?;
                let sha256 = <[u8; 32]>::from(Sha256::digest(&bytes));
                ReplaceDestination::Existing(
                    file.park_request(ExpectedFileContent::new(revision, sha256)),
                )
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ReplaceDestination::Vacant {
                    parent: self.inner.directory.clone(),
                    name: name.clone(),
                }
            }
            Err(error) => {
                self.discard_sealed_stage_locked(&transition, sealed)?;
                return Err(error.into());
            }
        };
        match sealed.replace_nondurable(destination) {
            FileReplaceOutcome::Replaced { current, displaced } => {
                if let Some(displaced) = displaced {
                    retain_parked_file_removal_locked(&transition, displaced);
                    root.settle_locked(&transition)?;
                }
                self.guard_from_file(name, current)
            }
            FileReplaceOutcome::NoEffect {
                error,
                staged,
                destination: _,
            } => {
                self.discard_sealed_stage_locked(&transition, staged)?;
                Err(error.into())
            }
            FileReplaceOutcome::AppliedUnverified(obligation) => {
                let receipt = root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_file_replace,
                );
                let _settlement = root.effects.settle();
                match receipt.claim() {
                    FileReplaceReceiptOutcome::Replaced { current, displaced } => {
                        if let Some(displaced) = displaced {
                            retain_parked_file_removal_locked(&transition, displaced);
                        }
                        let guard = self.guard_from_file(name, current)?;
                        root.settle_locked(&transition)?;
                        Ok(guard)
                    }
                    FileReplaceReceiptOutcome::NoEffect {
                        staged,
                        destination,
                    } => {
                        drop(destination);
                        self.discard_sealed_stage_locked(&transition, staged)?;
                        Err(unsettled("managed replacement had no effect"))
                    }
                    FileReplaceReceiptOutcome::Pending(receipt) => {
                        root.retain_continuation_locked(
                            &transition,
                            ManagedEffectContinuation::FileReplace(receipt),
                        );
                        Err(unsettled("managed replacement remains unsettled"))
                    }
                }
            }
        }
    }

    pub(crate) async fn write_exact(
        &self,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        self.write_exact_blocking(name, bytes)
    }

    fn write_exact_blocking(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        let name_leaf = leaf(name)?;
        let mut staged = self.create_stage()?;
        if let Err(error) = staged.write_all(bytes) {
            self.discard_stage(staged);
            return Err(error.into());
        }
        let sealed = self.seal_stage(staged)?;
        let guard = self.replace_or_promote(name_leaf, sealed)?;
        if self.read_guarded_file_bounded(name, &guard, bytes.len() as u64)? != bytes {
            return Err(LoaderError::Verify(
                "managed replacement changed after publication".to_string(),
            ));
        }
        self.sync()
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn write_exact_fixture(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        self.write_exact_blocking(name, bytes)
    }

    pub(crate) async fn write_relative_exact(
        &self,
        relative: &PortableRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        let (parent, name) = self.open_or_create_relative_parent(relative)?;
        parent.write_exact(&name, bytes).await
    }

    pub(crate) fn rename_guarded_file_no_replace(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        destination: &ManagedDir,
        destination_name: &str,
    ) -> Result<(), LoaderError> {
        let source_name = leaf(name)?;
        let destination_name = leaf(destination_name)?;
        if !Arc::ptr_eq(&self.inner.root, &destination.inner.root) {
            return Err(LoaderError::Verify(
                "managed file move crosses root authorities".to_string(),
            ));
        }
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.revalidate_locked(&transition)?;
        destination.revalidate_locked(&transition)?;
        let file = self.inner.directory.open_file(&source_name)?;
        if !guard.identity.matches(&file)? || file.validate_revision(&guard.revision).is_err() {
            return Err(LoaderError::Verify(
                "managed file move source changed before publication".to_string(),
            ));
        }
        match file.move_no_replace(&destination.inner.directory, &destination_name) {
            FileMoveOutcome::Applied(file) => {
                guard.identity.replace_capability(file);
            }
            FileMoveOutcome::NoEffect { error, file } => {
                guard.identity.replace_capability(file);
                return Err(error.into());
            }
            FileMoveOutcome::AppliedUnverified(obligation) => {
                guard.identity.mark_unsettled();
                let receipt = root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_file_move,
                );
                let _settlement = root.effects.settle();
                match receipt.claim() {
                    FileMoveReceiptOutcome::Applied(file) => {
                        guard.identity.replace_capability(file);
                        root.settle_locked(&transition)?;
                    }
                    FileMoveReceiptOutcome::NoEffect(file) => {
                        guard.identity.replace_capability(file);
                        root.settle_locked(&transition)?;
                        return Err(unsettled("managed file move had no effect"));
                    }
                    FileMoveReceiptOutcome::Pending(receipt) => {
                        root.retain_continuation_locked(
                            &transition,
                            ManagedEffectContinuation::FileMove {
                                receipt,
                                identity: guard.identity.pinless_proof(),
                            },
                        );
                        return Err(unsettled("managed file move remains unsettled"));
                    }
                }
            }
        }
        if !destination.file_guard_matches_locked(
            &transition,
            destination_name.as_os_str().to_str().ok_or_else(|| {
                LoaderError::Verify("managed destination name is not UTF-8".to_string())
            })?,
            guard,
        )? {
            return Err(LoaderError::Verify(
                "managed file move destination changed after publication".to_string(),
            ));
        }
        self.inner.directory.sync()?;
        destination.inner.directory.sync()?;
        Ok(())
    }

    pub(crate) fn move_child_guarded_no_replace(
        &self,
        name: &str,
        child: ManagedDir,
        destination: &ManagedDir,
        destination_name: &str,
    ) -> Result<ManagedDir, ManagedDirectoryMoveFailure> {
        let source_name = leaf(name).map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        let destination_leaf =
            leaf(destination_name).map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        if !Arc::ptr_eq(&self.inner.root, &destination.inner.root) {
            return Err(ManagedDirectoryMoveFailure::BeforeMove);
        }
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)
            .map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        self.revalidate_locked(&transition)
            .map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        destination
            .revalidate_locked(&transition)
            .map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        child
            .revalidate_locked(&transition)
            .map_err(|_| ManagedDirectoryMoveFailure::BeforeMove)?;
        if !Arc::ptr_eq(&self.inner.root, &destination.inner.root)
            || child.inner.path != self.inner.path.join(source_name.as_os_str())
            || child.inner.is_root
        {
            return Err(ManagedDirectoryMoveFailure::BeforeMove);
        }
        match child
            .inner
            .directory
            .clone()
            .move_no_replace(&destination.inner.directory, &destination_leaf)
        {
            DirectoryMoveOutcome::Applied(directory) => destination
                .child_from_directory(destination_name, directory)
                .map_err(|_| ManagedDirectoryMoveFailure::MoveAttempted),
            DirectoryMoveOutcome::NoEffect {
                error: _,
                directory: _,
            } => Err(ManagedDirectoryMoveFailure::MoveAttempted),
            DirectoryMoveOutcome::AppliedUnverified(obligation) => {
                let receipt = root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_directory_move,
                );
                let _settlement = root.effects.settle();
                match receipt.claim() {
                    DirectoryMoveReceiptOutcome::Applied(directory) => {
                        let moved = destination
                            .child_from_directory(destination_name, directory)
                            .map_err(|_| ManagedDirectoryMoveFailure::MoveAttempted)?;
                        root.settle_locked(&transition)
                            .map_err(|_| ManagedDirectoryMoveFailure::MoveAttempted)?;
                        Ok(moved)
                    }
                    DirectoryMoveReceiptOutcome::NoEffect(directory) => {
                        drop(directory);
                        root.settle_locked(&transition)
                            .map_err(|_| ManagedDirectoryMoveFailure::MoveAttempted)?;
                        Err(ManagedDirectoryMoveFailure::MoveAttempted)
                    }
                    DirectoryMoveReceiptOutcome::Pending(receipt) => {
                        root.retain_continuation_locked(
                            &transition,
                            ManagedEffectContinuation::DirectoryMove(receipt),
                        );
                        Err(ManagedDirectoryMoveFailure::MoveAttempted)
                    }
                }
            }
        }
    }

    pub(crate) fn remove_guarded_file(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<(), LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.remove_guarded_file_locked(&transition, name, guard)
    }

    fn remove_guarded_file_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<(), LoaderError> {
        let name_leaf = leaf(name)?;
        if !self.file_guard_matches_locked(transition, name, guard)? {
            return Err(LoaderError::Verify(
                "managed file removal source changed".to_string(),
            ));
        }
        let file = self.inner.directory.open_file(&name_leaf)?;
        if !guard.identity.matches(&file)? {
            return Err(LoaderError::Verify(
                "managed file removal source changed before parking".to_string(),
            ));
        }
        let revision = file.revision()?;
        let bytes = file.read_bounded(MAX_MANAGED_READ_BYTES)?;
        file.validate_revision(&revision)?;
        let expected = ExpectedFileContent::new(
            revision,
            <[u8; 32]>::from(Sha256::digest(&bytes)),
        );
        match self.inner.directory.park_file(file.park_request(expected)) {
            FileParkOutcome::Parked(parked) => {
                retain_parked_file_removal_locked(transition, parked);
                self.inner.root.settle_locked(transition)?;
            }
            FileParkOutcome::NoEffect { error, request: _ } => return Err(error.into()),
            FileParkOutcome::AppliedUnverified(obligation) => {
                self.inner.root.retain_linear_locked(
                    transition,
                    obligation,
                    EffectOwner::retain_file_park_removal,
                );
                self.inner.root.settle_locked(transition)?;
                return Err(unsettled("managed file removal remains unsettled"));
            }
        }
        Ok(())
    }

    pub(crate) fn remove_empty_child_guarded(
        &self,
        name: &str,
        park_name: &str,
        child: ManagedDir,
    ) -> Result<ManagedEmptyChildRemoval, LoaderError> {
        let _name = leaf(name)?;
        let park_name = leaf(park_name)?;
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        if !Arc::ptr_eq(&root, &child.inner.root)
            || child.inner.path != self.inner.path.join(name)
            || child.inner.is_root
        {
            return Err(LoaderError::Verify(
                "managed empty-directory removal target is not the admitted child".to_string(),
            ));
        }
        if !child.listing_locked(&transition, 1)?.is_empty() {
            return Err(LoaderError::Verify(
                "managed empty-directory removal target is not empty".to_string(),
            ));
        }
        match child.inner.directory.clone().park_as(park_name) {
            DirectoryParkOutcome::Parked(parked) => {
                retain_parked_directory_removal_locked(&transition, parked);
                root.settle_locked(&transition)?;
                Ok(ManagedEmptyChildRemoval::Removed)
            }
            DirectoryParkOutcome::NoEffect {
                error,
                directory: _,
            } => Err(error.into()),
            DirectoryParkOutcome::AppliedUnverified(obligation) => {
                root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_directory_park_removal,
                );
                root.settle_locked(&transition)?;
                Err(unsettled("managed directory removal remains unsettled"))
            }
        }
    }

    fn remove_empty_child(&self, child: &ManagedDir) -> Result<(), LoaderError> {
        let root = self.inner.root.clone();
        let transition = root.transition();
        root.settle_locked(&transition)?;
        self.remove_empty_child_locked(&transition, child)
    }

    fn remove_empty_child_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        child: &ManagedDir,
    ) -> Result<(), LoaderError> {
        self.inner.root.require_transition(transition);
        if child.inner.path.parent() != Some(self.inner.path.as_path()) || child.inner.is_root {
            return Err(LoaderError::Verify(
                "managed directory removal target is not a child".to_string(),
            ));
        }
        match child.inner.directory.clone().park() {
            DirectoryParkOutcome::Parked(parked) => match parked.remove_empty() {
                DirectoryRemovalOutcome::Removed => Ok(()),
                DirectoryRemovalOutcome::NoEffect { error, parked } => {
                    self.inner.root.retain_linear_locked(
                        transition,
                        parked,
                        EffectOwner::retain_parked_directory_removal,
                    );
                    self.inner.root.settle_locked(transition)?;
                    Err(error.into())
                }
                DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
                    self.inner.root.retain_linear_locked(
                        transition,
                        obligation,
                        EffectOwner::retain_directory_removal,
                    );
                    self.inner.root.settle_locked(transition)?;
                    Err(unsettled("managed directory removal remains unsettled"))
                }
            },
            DirectoryParkOutcome::NoEffect {
                error,
                directory: _,
            } => Err(error.into()),
            DirectoryParkOutcome::AppliedUnverified(obligation) => {
                self.inner.root.retain_linear_locked(
                    transition,
                    obligation,
                    EffectOwner::retain_directory_park_removal,
                );
                self.inner.root.settle_locked(transition)?;
                Err(unsettled("managed directory park remains unsettled"))
            }
        }
    }

    pub(crate) fn clear_owned_contents(self) -> Result<(), LoaderError> {
        if self.inner.is_root {
            return Err(LoaderError::Verify(
                "managed root cannot be recursively cleared".to_string(),
            ));
        }
        self.clear_contents(0)
    }

    fn clear_contents(&self, depth: usize) -> Result<(), LoaderError> {
        if depth > MAX_MANAGED_TREE_OPERATION_DEPTH {
            return Err(LoaderError::Verify(
                "managed cleanup tree exceeds its depth bound".to_string(),
            ));
        }
        for entry in self.listing(MAX_MANAGED_TREE_OPERATION_ENTRIES)? {
            let name = entry.utf8_name().ok_or_else(|| {
                LoaderError::Verify("managed cleanup contains a non-UTF-8 name".to_string())
            })?;
            PortableFileName::new_exact(name).map_err(|_| {
                LoaderError::Verify("managed cleanup contains a non-portable name".to_string())
            })?;
            match entry.kind() {
                EntryKind::File => {
                    let guard = self.inspect_regular_file(name)?.ok_or_else(|| {
                        LoaderError::Verify("managed cleanup file disappeared".to_string())
                    })?;
                    self.remove_guarded_file(name, &guard)?;
                }
                EntryKind::Directory => {
                    let child = self.open_observed_child(&entry)?;
                    child.clear_contents(depth + 1)?;
                    self.remove_empty_child(&child)?;
                }
                EntryKind::Link | EntryKind::Other => {
                    return Err(LoaderError::Verify(
                        "managed cleanup refuses links and unsupported entries".to_string(),
                    ));
                }
            }
        }
        self.revalidate()
    }

    fn clear_contents_locked(
        &self,
        transition: &ManagedEffectTransition<'_>,
        depth: usize,
    ) -> Result<(), LoaderError> {
        if depth > MAX_MANAGED_TREE_OPERATION_DEPTH {
            return Err(LoaderError::Verify(
                "managed cleanup tree exceeds its depth bound".to_string(),
            ));
        }
        for entry in self.listing_locked(transition, MAX_MANAGED_TREE_OPERATION_ENTRIES)? {
            let name = entry.utf8_name().ok_or_else(|| {
                LoaderError::Verify("managed cleanup contains a non-UTF-8 name".to_string())
            })?;
            PortableFileName::new_exact(name).map_err(|_| {
                LoaderError::Verify("managed cleanup contains a non-portable name".to_string())
            })?;
            match entry.kind() {
                EntryKind::File => {
                    let guard = self
                        .inspect_regular_file_locked(transition, name)?
                        .ok_or_else(|| {
                            LoaderError::Verify(
                                "managed cleanup file disappeared".to_string(),
                            )
                        })?;
                    self.remove_guarded_file_locked(transition, name, &guard)?;
                }
                EntryKind::Directory => {
                    let child = self.open_observed_child(&entry)?;
                    child.clear_contents_locked(transition, depth + 1)?;
                    self.remove_empty_child_locked(transition, &child)?;
                }
                EntryKind::Link | EntryKind::Other => {
                    return Err(LoaderError::Verify(
                        "managed cleanup refuses links and unsupported entries".to_string(),
                    ));
                }
            }
        }
        self.revalidate_locked(transition)
    }

    pub(crate) fn verify_authenticated(
        &self,
        name: &str,
        expected_size: u64,
        expected_sha1: &str,
    ) -> Result<(), LoaderError> {
        let guard = self.inspect_regular_file(name)?.ok_or_else(|| {
            LoaderError::Verify("managed authenticated file is absent".to_string())
        })?;
        if guard.size != expected_size
            || !self
                .sha1_guarded_file(name, &guard, expected_size)?
                .eq_ignore_ascii_case(expected_sha1)
        {
            return Err(LoaderError::Verify(
                "managed authenticated file failed integrity verification".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn read_authenticated(
        &self,
        name: &str,
        expected_size: Option<u64>,
        expected_sha1: Option<&str>,
    ) -> Result<Vec<u8>, LoaderError> {
        let guard = self.inspect_regular_file(name)?.ok_or_else(|| {
            LoaderError::Verify("managed authenticated file is absent".to_string())
        })?;
        let limit = expected_size.unwrap_or(MAX_MANAGED_READ_BYTES);
        if expected_size.is_some_and(|size| size != guard.size) {
            return Err(LoaderError::Verify(
                "managed authenticated file has the wrong size".to_string(),
            ));
        }
        let bytes = self.read_guarded_file_bounded(name, &guard, limit)?;
        if expected_sha1.is_some_and(|expected| {
            !hex_lower(&<[u8; 20]>::from(Sha1::digest(&bytes))).eq_ignore_ascii_case(expected)
        }) {
            return Err(LoaderError::Verify(
                "managed authenticated file failed integrity verification".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn read_relative_authenticated(
        &self,
        relative: &PortableRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        let mut segments = relative.as_str().split('/').peekable();
        let mut directory = self.clone();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                let bytes = directory.read_authenticated(segment, expected_size, None)?;
                if <[u8; 20]>::from(Sha1::digest(&bytes)) != *expected_sha1 {
                    return Err(LoaderError::Verify(
                        "managed relative file failed integrity verification".to_string(),
                    ));
                }
                return Ok(bytes);
            }
            directory = directory.open_child(segment)?;
        }
        Err(LoaderError::Verify(
            "managed relative path has no file name".to_string(),
        ))
    }

    pub(crate) async fn import_relative_authenticated<R>(
        &self,
        relative: &PortableRelativePath,
        source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<(), LoaderError>
    where
        R: Read + Seek + Send + 'static,
    {
        let (parent, name) = self.open_or_create_relative_parent(relative)?;
        parent
            .import_authenticated_inner(
                name,
                source,
                expected_size,
                expected_sha1,
                true,
                (),
                #[cfg(test)]
                None,
            )
            .await
            .map(drop)
    }

    pub(crate) async fn import_authenticated_create_new<R, G>(
        &self,
        name: &str,
        source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
        lifetime_guard: G,
    ) -> Result<ManagedFileIdentity, LoaderError>
    where
        R: Read + Seek + Send + 'static,
        G: Send + 'static,
    {
        self.import_authenticated_inner(
            name.to_string(),
            source,
            expected_size,
            expected_sha1,
            false,
            lifetime_guard,
            #[cfg(test)]
            None,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn import_authenticated_create_new_with_hook<R, G>(
        &self,
        name: &str,
        source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
        lifetime_guard: G,
        blocking_hook: Box<dyn FnOnce() + Send + 'static>,
    ) -> Result<ManagedFileIdentity, LoaderError>
    where
        R: Read + Seek + Send + 'static,
        G: Send + 'static,
    {
        self.import_authenticated_inner(
            name.to_string(),
            source,
            expected_size,
            expected_sha1,
            false,
            lifetime_guard,
            Some(blocking_hook),
        )
        .await
    }

    async fn import_authenticated_inner<R, G>(
        &self,
        name: String,
        source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
        replace_existing: bool,
        lifetime_guard: G,
        #[cfg(test)] blocking_hook: Option<Box<dyn FnOnce() + Send + 'static>>,
    ) -> Result<ManagedFileIdentity, LoaderError>
    where
        R: Read + Seek + Send + 'static,
        G: Send + 'static,
    {
        if expected_size > MAX_MANAGED_READ_BYTES {
            return Err(LoaderError::Verify(
                "managed authenticated import exceeds its size bound".to_string(),
            ));
        }
        let directory = self.clone();
        tokio::task::spawn_blocking(move || {
            let _lifetime_guard = lifetime_guard;
            #[cfg(test)]
            if let Some(hook) = blocking_hook {
                hook();
            }
            directory.import_authenticated(
                &name,
                source,
                expected_size,
                expected_sha1,
                replace_existing,
            )
        })
        .await
        .map_err(|_| {
            LoaderError::Verify("managed authenticated import worker stopped".to_string())
        })?
    }

    fn import_authenticated<R: Read + Seek>(
        &self,
        name: &str,
        mut source: R,
        expected_size: u64,
        expected_sha1: [u8; 20],
        replace_existing: bool,
    ) -> Result<ManagedFileIdentity, LoaderError> {
        let name_leaf = leaf(name)?;
        source.seek(SeekFrom::Start(0))?;
        let mut staged = self.create_stage()?;
        let transfer = (|| -> Result<[u8; 20], LoaderError> {
            let mut writer = staged.writer()?;
            let mut hasher = Sha1::new();
            let mut observed = 0_u64;
            let mut chunk = [0_u8; 64 * 1024];
            loop {
                let read = source.read(&mut chunk)?;
                if read == 0 {
                    break;
                }
                observed = observed.checked_add(read as u64).ok_or_else(|| {
                    LoaderError::Verify("managed authenticated import size overflowed".to_string())
                })?;
                if observed > expected_size {
                    return Err(LoaderError::Verify(
                        "managed authenticated import exceeds its declared size".to_string(),
                    ));
                }
                writer.write_all(&chunk[..read])?;
                hasher.update(&chunk[..read]);
            }
            writer.finish()?;
            if observed != expected_size {
                return Err(LoaderError::Verify(
                    "managed authenticated import has the wrong size".to_string(),
                ));
            }
            Ok(<[u8; 20]>::from(hasher.finalize()))
        })();
        let observed_sha1 = match transfer {
            Ok(digest) => digest,
            Err(error) => {
                self.discard_stage(staged);
                return Err(error);
            }
        };
        if observed_sha1 != expected_sha1 {
            self.discard_stage(staged);
            return Err(LoaderError::Verify(
                "managed authenticated import failed integrity verification".to_string(),
            ));
        }
        let sealed = self.seal_stage(staged)?;
        let guard = if replace_existing {
            self.replace_or_promote(name_leaf, sealed)?
        } else {
            self.promote_create_new(name_leaf, sealed).map_err(|failure| match failure {
                ManagedCreateOnlyWriteFailure::BeforePromotion(error) => error,
                ManagedCreateOnlyWriteFailure::PromotionAttempted { .. } => unsettled(
                    "managed authenticated create-only publication could not be classified",
                ),
            })?
        };
        if guard.size != expected_size
            || self.sha1_guarded_file_bytes(name, &guard, expected_size)? != expected_sha1
        {
            return Err(LoaderError::Verify(
                "managed authenticated import changed after publication".to_string(),
            ));
        }
        Ok(guard.identity())
    }

    pub(crate) fn validate_exact_child_directories(
        &self,
        expected: &[&str],
    ) -> Result<(), LoaderError> {
        let expected = expected
            .iter()
            .map(|name| portable_key(name).map(|key| (key, *name)))
            .collect::<Result<HashMap<_, _>, _>>()?;
        let entries = self.listing(expected.len().saturating_add(1))?;
        if entries.len() != expected.len() {
            return Err(LoaderError::Verify(
                "managed directory contains unexpected children".to_string(),
            ));
        }
        for entry in entries {
            let name = entry.utf8_name().ok_or_else(|| {
                LoaderError::Verify("managed directory contains a non-UTF-8 name".to_string())
            })?;
            let Some(exact) = expected.get(&portable_key(name)?) else {
                return Err(LoaderError::Verify(
                    "managed directory contains an unexpected child".to_string(),
                ));
            };
            if name != *exact || entry.kind() != EntryKind::Directory {
                return Err(LoaderError::Verify(
                    "managed directory child is not exact".to_string(),
                ));
            }
            self.open_observed_child(&entry)?;
        }
        Ok(())
    }

    pub(crate) fn validate_tree_usage_no_links(
        &self,
        limits: ManagedTreeLimits,
    ) -> Result<ManagedTreeUsage, LoaderError> {
        self.validate_tree_usage(limits, false)
    }

    pub(crate) fn validate_tree_usage_allow_links(
        &self,
        limits: ManagedTreeLimits,
    ) -> Result<ManagedTreeUsage, LoaderError> {
        self.validate_tree_usage(limits, true)
    }

    fn validate_tree_usage(
        &self,
        limits: ManagedTreeLimits,
        allow_links: bool,
    ) -> Result<ManagedTreeUsage, LoaderError> {
        let mut state = ManagedTreeCaptureState::new(limits);
        self.capture_tree_directory(None, 0, &mut state, None, allow_links)?;
        Ok(ManagedTreeUsage {
            entries: limits.max_entries - state.remaining_entries,
            bytes: limits.max_total_bytes - state.remaining_bytes,
        })
    }

    pub(crate) fn snapshot_tree(
        &self,
        limits: ManagedTreeLimits,
    ) -> Result<ManagedTreeSnapshot, LoaderError> {
        let first = self.capture_tree(limits)?;
        let second = self.capture_tree(limits)?;
        if first != second {
            return Err(LoaderError::Verify(
                "managed tree changed during snapshot".to_string(),
            ));
        }
        Ok(second)
    }

    fn capture_tree(&self, limits: ManagedTreeLimits) -> Result<ManagedTreeSnapshot, LoaderError> {
        let mut state = ManagedTreeCaptureState::new(limits);
        let mut snapshot = ManagedTreeSnapshot::default();
        self.capture_tree_directory(None, 0, &mut state, Some(&mut snapshot), false)?;
        Ok(snapshot)
    }

    fn capture_tree_directory(
        &self,
        prefix: Option<&str>,
        depth: usize,
        state: &mut ManagedTreeCaptureState,
        mut snapshot: Option<&mut ManagedTreeSnapshot>,
        allow_links: bool,
    ) -> Result<(), LoaderError> {
        if depth > state.limits.max_depth {
            return Err(LoaderError::Verify(
                "managed tree exceeds its depth bound".to_string(),
            ));
        }
        let scan_limit = state.remaining_entries.saturating_add(1).max(1);
        let mut entries = self.listing(scan_limit)?;
        entries.sort_by(|left, right| left.name().cmp(right.name()));
        if entries.len() > state.remaining_entries {
            return Err(LoaderError::Verify(
                "managed tree exceeds its entry bound".to_string(),
            ));
        }
        state.remaining_entries -= entries.len();
        for entry in entries {
            let name = entry.utf8_name().ok_or_else(|| {
                LoaderError::Verify("managed tree contains a non-UTF-8 name".to_string())
            })?;
            let authored = prefix.map_or_else(|| name.to_string(), |prefix| format!("{prefix}/{name}"));
            let relative = PortableRelativePath::new_exact(&authored).map_err(|_| {
                LoaderError::Verify("managed tree contains a non-portable path".to_string())
            })?;
            let key = relative.key();
            if state.aliases.insert(key, authored.clone()).is_some() {
                return Err(LoaderError::Verify(
                    "managed tree contains a portable path alias".to_string(),
                ));
            }
            match entry.kind() {
                EntryKind::File => {
                    let guard = self.inspect_regular_file(name)?.ok_or_else(|| {
                        LoaderError::Verify("managed tree file disappeared".to_string())
                    })?;
                    if guard.size > state.limits.max_file_bytes
                        || guard.size > state.remaining_bytes
                    {
                        return Err(LoaderError::Verify(
                            "managed tree file exceeds its byte bound".to_string(),
                        ));
                    }
                    state.remaining_bytes -= guard.size;
                    if let Some(snapshot) = snapshot.as_deref_mut() {
                        let sha1 = self.sha1_guarded_file_bytes(
                            name,
                            &guard,
                            state.limits.max_file_bytes,
                        )?;
                        snapshot.files.insert(
                            relative,
                            ManagedFileFact {
                                size: guard.size,
                                sha1,
                            },
                        );
                    }
                }
                EntryKind::Directory => {
                    if depth == state.limits.max_depth {
                        return Err(LoaderError::Verify(
                            "managed tree exceeds its depth bound".to_string(),
                        ));
                    }
                    if let Some(snapshot) = snapshot.as_deref_mut() {
                        snapshot.directories.insert(relative);
                    }
                    let child = self.open_observed_child(&entry)?;
                    child.capture_tree_directory(
                        Some(&authored),
                        depth + 1,
                        state,
                        snapshot.as_deref_mut(),
                        allow_links,
                    )?;
                }
                EntryKind::Link if allow_links => {}
                EntryKind::Link | EntryKind::Other => {
                    return Err(LoaderError::Verify(
                        "managed tree contains a link or unsupported entry".to_string(),
                    ));
                }
            }
        }
        self.revalidate()
    }
}

struct ManagedTreeCaptureState {
    limits: ManagedTreeLimits,
    remaining_entries: usize,
    remaining_bytes: u64,
    aliases: HashMap<PortablePathKey, String>,
}

impl ManagedTreeCaptureState {
    fn new(limits: ManagedTreeLimits) -> Self {
        Self {
            limits,
            remaining_entries: limits.max_entries,
            remaining_bytes: limits.max_total_bytes,
            aliases: HashMap::new(),
        }
    }
}

impl ManagedDir {
    pub(crate) fn open_or_create_persistent_file(
        &self,
        name: &str,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        let guard = match self.inspect_regular_file(name)? {
            Some(guard) => guard,
            None => match self.write_new_exact_retained(name, &[]) {
                Ok(guard) => guard,
                Err(ManagedCreateOnlyWriteFailure::BeforePromotion(LoaderError::Io(error)))
                    if error.kind() == io::ErrorKind::AlreadyExists =>
                {
                    self.inspect_regular_file(name)?.ok_or_else(|| {
                        LoaderError::Verify(
                            "managed persistent file disappeared during creation".to_string(),
                        )
                    })?
                }
                Err(ManagedCreateOnlyWriteFailure::BeforePromotion(error)) => return Err(error),
                Err(ManagedCreateOnlyWriteFailure::PromotionAttempted { final_guard }) => {
                    final_guard.ok_or_else(|| {
                        unsettled("managed persistent file creation remains unsettled")
                    })?
                }
            },
        };
        self.bind_persistent_file(name, guard)
    }

    pub(crate) fn open_persistent_file(
        &self,
        name: &str,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        let guard = self.inspect_regular_file(name)?.ok_or_else(|| {
            LoaderError::Io(io::Error::new(
                io::ErrorKind::NotFound,
                "managed persistent file is absent",
            ))
        })?;
        self.bind_persistent_file(name, guard)
    }

    fn bind_persistent_file(
        &self,
        name: &str,
        guard: ManagedFileGuard,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        let lock = self
            .inner
            .root
            .publication_lock(self.inner.identity, name)?;
        let persistent = ManagedPersistentFile {
            directory: self.clone(),
            name: name.to_string(),
            identity: guard.identity(),
            lock,
            held: Mutex::new(None),
        };
        persistent.revalidate()?;
        Ok(persistent)
    }
}

impl ManagedPersistentFile {
    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        let guard = self
            .directory
            .inspect_regular_file(&self.name)?
            .ok_or_else(|| {
                LoaderError::Verify("managed persistent file is absent".to_string())
            })?;
        if guard.identity() != self.identity {
            return Err(LoaderError::Verify(
                "managed persistent file changed identity".to_string(),
            ));
        }
        Ok(())
    }

    pub(crate) fn try_lock_exclusive(&self) -> Result<bool, LoaderError> {
        self.revalidate()?;
        let mut held = self.held.lock().map_err(|_| {
            LoaderError::Verify("managed persistent lock state was poisoned".to_string())
        })?;
        if held.is_some() {
            return Err(LoaderError::Verify(
                "managed persistent lock is already held by this handle".to_string(),
            ));
        }
        let mut state = self.lock.state.lock().map_err(|_| {
            LoaderError::Verify("managed publication lock was poisoned".to_string())
        })?;
        if state.writer || state.readers != 0 {
            return Ok(false);
        }
        state.writer = true;
        *held = Some(PublicationLockMode::Exclusive);
        Ok(true)
    }

    pub(crate) fn try_lock_shared(&self) -> Result<bool, LoaderError> {
        self.revalidate()?;
        let mut held = self.held.lock().map_err(|_| {
            LoaderError::Verify("managed persistent lock state was poisoned".to_string())
        })?;
        if held.is_some() {
            return Err(LoaderError::Verify(
                "managed persistent lock is already held by this handle".to_string(),
            ));
        }
        let mut state = self.lock.state.lock().map_err(|_| {
            LoaderError::Verify("managed publication lock was poisoned".to_string())
        })?;
        if state.writer {
            return Ok(false);
        }
        state.readers = state.readers.checked_add(1).ok_or_else(|| {
            LoaderError::Verify("managed publication reader count overflowed".to_string())
        })?;
        *held = Some(PublicationLockMode::Shared);
        Ok(true)
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        let mode = self
            .held
            .lock()
            .map_err(|_| io::Error::other("managed persistent lock state was poisoned"))?
            .take()
            .ok_or_else(|| io::Error::other("managed persistent lock is not held"))?;
        let mut state = self
            .lock
            .state
            .lock()
            .map_err(|_| io::Error::other("managed publication lock was poisoned"))?;
        match mode {
            PublicationLockMode::Shared => {
                state.readers = state.readers.checked_sub(1).ok_or_else(|| {
                    io::Error::other("managed publication reader count underflowed")
                })?;
            }
            PublicationLockMode::Exclusive => {
                if !state.writer {
                    return Err(io::Error::other(
                        "managed publication writer state was not held",
                    ));
                }
                state.writer = false;
            }
        }
        Ok(())
    }
}

impl Drop for ManagedPersistentFile {
    fn drop(&mut self) {
        if self
            .held
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .is_some()
        {
            let _ = self.unlock();
        }
    }
}

#[cfg(test)]
fn injected_create_only_write_failure() -> LoaderError {
    LoaderError::Io(io::Error::other("injected managed create-only write failure"))
}

fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    let mut encoded = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

fn unsettled(message: &'static str) -> LoaderError {
    LoaderError::Io(io::Error::new(io::ErrorKind::WouldBlock, message))
}

static ROOTS: OnceLock<Mutex<HashMap<PathBuf, Weak<ManagedRoot>>>> = OnceLock::new();

fn roots() -> &'static Mutex<HashMap<PathBuf, Weak<ManagedRoot>>> {
    ROOTS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn absolute_root_key(path: &Path) -> Result<PathBuf, LoaderError> {
    let path = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    return Err(LoaderError::Verify(
                        "managed root escapes its absolute namespace".to_string(),
                    ));
                }
            }
        }
    }
    Ok(normalized)
}

fn acquire_root_session(path: &Path) -> Result<RootSession, LoaderError> {
    settle_root_session_acquisition(RootSession::acquire(path))
}

fn settle_root_session_acquisition(
    mut outcome: RootSessionAcquireOutcome,
) -> Result<RootSession, LoaderError> {
    for _ in 0..3 {
        outcome = match outcome {
            RootSessionAcquireOutcome::Acquired(session) => return Ok(session),
            RootSessionAcquireOutcome::NoEffect(error) => {
                return Err(LoaderError::Verify(error.to_string()));
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation.reconcile(),
        };
    }
    match outcome {
        RootSessionAcquireOutcome::Acquired(session) => Ok(session),
        RootSessionAcquireOutcome::NoEffect(error) => Err(LoaderError::Verify(error.to_string())),
        RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let error = obligation.error().to_string();
            if obligation.cleanup().is_err() {
                std::process::abort();
            }
            Err(LoaderError::Verify(error))
        }
    }
}

fn settle_admitted_root_session_acquisition(
    mut outcome: AdmittedRootSessionAcquireOutcome,
) -> Result<AdmittedRootSession, LoaderError> {
    for _ in 0..3 {
        outcome = match outcome {
            AdmittedRootSessionAcquireOutcome::Acquired(session) => return Ok(session),
            AdmittedRootSessionAcquireOutcome::NoEffect(error) => {
                return Err(LoaderError::Verify(error.to_string()));
            }
            AdmittedRootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                obligation.reconcile()
            }
        };
    }
    match outcome {
        AdmittedRootSessionAcquireOutcome::Acquired(session) => Ok(session),
        AdmittedRootSessionAcquireOutcome::NoEffect(error) => {
            Err(LoaderError::Verify(error.to_string()))
        }
        AdmittedRootSessionAcquireOutcome::AppliedUnverified(obligation) => {
            let error = obligation.error().to_string();
            if obligation.cleanup().is_err() {
                std::process::abort();
            }
            Err(LoaderError::Verify(error))
        }
    }
}

impl ManagedLibraryRoot {
    pub fn admitted_binding(
        admission: &AdmittedAbsoluteDirectory,
    ) -> io::Result<ManagedLibraryBinding> {
        admission
            .filesystem_identity()
            .map(ManagedLibraryBinding)
    }

    pub fn from_admitted_directory(admission: AdmittedAbsoluteDirectory) -> io::Result<Self> {
        admission.revalidate()?;
        let admitted_identity = admission.filesystem_identity()?;
        let session = settle_admitted_root_session_acquisition(admission.acquire_root_session()?)
            .map_err(loader_io)?;
        let directory = session.root()?;
        if admitted_identity != directory.identity()?.filesystem_identity() {
            drop(directory);
            drop(session);
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "managed library lease does not match the admitted directory",
            ));
        }
        let effects = directory.create_effect_owner()?;
        admission.revalidate()?;
        let root = ManagedDir::from_directory_with_session(
            directory,
            effects,
            Some(ManagedRootSession::Admitted(session)),
        )
        .map_err(loader_io)?;
        root.settle().map_err(loader_io)?;
        admission.revalidate()?;
        Self::finish_construction(root, ManagedLibraryAdmission::App(admission))
    }

    fn finish_construction(
        root: ManagedDir,
        admission: ManagedLibraryAdmission,
    ) -> io::Result<Self> {
        let admission = Arc::new(ManagedLibraryAdmissionVerifier {
            state: RwLock::new(Arc::new(ManagedLibraryAdmissionState {
                current: admission,
                epoch: 0,
            })),
            root_identity: root.inner.identity.filesystem_identity(),
            test_root: Arc::downgrade(&root.inner.root),
        });
        let managed = Self {
            authority: Arc::new(ManagedLibraryAuthority {
                root,
                admission,
                lifecycle: ManagedAuthorityLifecycle::new(),
            }),
        };
        managed.revalidate()?;
        Ok(managed)
    }

    #[cfg(test)]
    pub(crate) fn open_for_test(path: &Path) -> io::Result<Self> {
        let configured_path = absolute_root_key(path).map_err(loader_io)?;
        let session = acquire_root_session(&configured_path).map_err(loader_io)?;
        let directory = session.root()?;
        let effects = directory.create_effect_owner()?;
        let root = ManagedDir::from_directory_with_session(
            directory,
            effects,
            Some(ManagedRootSession::Direct(session)),
        )
        .map_err(loader_io)?;
        root.settle().map_err(loader_io)?;
        Self::finish_construction(
            root,
            ManagedLibraryAdmission::Test {
                path: Arc::new(configured_path),
            },
        )
    }

    pub fn try_acquire(&self) -> io::Result<ManagedLibraryOperation> {
        self.authority.try_acquire()
    }

    pub fn witness(&self) -> ManagedLibraryWitness {
        ManagedLibraryWitness {
            authority: Arc::downgrade(&self.authority),
        }
    }

    pub fn begin_retirement(self) -> ManagedLibraryRetirement {
        self.authority.close();
        ManagedLibraryRetirement {
            authority: Arc::clone(&self.authority),
        }
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.authority.revalidate()
    }

    pub fn binding(&self) -> io::Result<ManagedLibraryBinding> {
        self.revalidate()?;
        self.authority.admission_binding()
    }

    pub fn prepare_admission_rebind(
        &self,
        candidate: AdmittedAbsoluteDirectory,
    ) -> io::Result<PreparedManagedLibraryAdmissionRebind> {
        self.authority.prepare_admission_rebind(candidate)
    }

    pub(crate) fn owns_directory(&self, directory: &ManagedDir) -> bool {
        self.authority.root.shares_root(directory)
    }
}

impl ManagedLibraryAuthority {
    fn prepare_admission_rebind(
        self: &Arc<Self>,
        candidate: AdmittedAbsoluteDirectory,
    ) -> io::Result<PreparedManagedLibraryAdmissionRebind> {
        self.root
            .inner
            .root
            .validate_retained_authority()
            .map_err(loader_io)?;
        let operation = ManagedLibraryOperation {
            authority: Arc::clone(self),
            pin: self.acquire_open_pin()?,
        };
        let candidate_binding = ManagedLibraryRoot::admitted_binding(&candidate)?;
        let root_binding = ManagedLibraryBinding(
            operation
                .authority
                .root
                .inner
                .identity
                .filesystem_identity(),
        );
        if candidate_binding != root_binding {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "candidate library admission does not match the active generation",
            ));
        }
        let admission = operation.authority.admission_snapshot()?;
        if !matches!(&admission.current, ManagedLibraryAdmission::App(_)) {
                return Err(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "test library authority cannot rebind an application admission",
                ));
        }
        candidate.revalidate()?;
        operation
            .authority
            .root
            .inner
            .root
            .validate_retained_authority()
            .map_err(loader_io)?;
        if !operation.authority.admission_is_current(&admission)? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed library admission changed during rebind preparation",
            ));
        }
        Ok(PreparedManagedLibraryAdmissionRebind {
            operation,
            candidate: Some(candidate),
            expected_epoch: admission.epoch,
        })
    }
    fn close(&self) {
        self.lifecycle.close();
    }

    fn try_acquire(self: &Arc<Self>) -> io::Result<ManagedLibraryOperation> {
        self.revalidate()?;
        let operation = ManagedLibraryOperation {
            authority: Arc::clone(self),
            pin: self.acquire_open_pin()?,
        };
        operation.revalidate()?;
        Ok(operation)
    }

    fn acquire_open_pin(&self) -> io::Result<Arc<ManagedOperationPin>> {
        self.lifecycle
            .acquire_pin(Some(Arc::clone(&self.admission)))
    }

    fn revalidate(&self) -> io::Result<()> {
        self.admission.verify()?;
        self.root.revalidate().map_err(loader_io)?;
        self.admission.verify()
    }

    fn admission_binding(&self) -> io::Result<ManagedLibraryBinding> {
        self.admission.binding().map(ManagedLibraryBinding)
    }

    fn admission_snapshot(&self) -> io::Result<Arc<ManagedLibraryAdmissionState>> {
        self.admission.snapshot()
    }

    fn admission_is_current(
        &self,
        admission: &Arc<ManagedLibraryAdmissionState>,
    ) -> io::Result<bool> {
        self.admission.is_current(admission)
    }
}

impl ManagedLibraryAdmissionVerifier {
    fn snapshot(&self) -> io::Result<Arc<ManagedLibraryAdmissionState>> {
        let admission = self
            .state
            .read()
            .map_err(|_| io::Error::other("managed library admission lock was poisoned"))?;
        Ok(Arc::clone(&admission))
    }

    fn is_current(&self, admission: &Arc<ManagedLibraryAdmissionState>) -> io::Result<bool> {
        self.state
            .read()
            .map(|current| Arc::ptr_eq(&current, admission))
            .map_err(|_| io::Error::other("managed library admission lock was poisoned"))
    }

    fn verify(&self) -> io::Result<()> {
        for _ in 0..3 {
            let admission = self.snapshot()?;
            admission
                .current
                .revalidate(self.root_identity, &self.test_root)?;
            if self.is_current(&admission)? {
                return Ok(());
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed library admission changed during revalidation",
        ))
    }

    fn binding(&self) -> io::Result<axial_fs::DirectoryFilesystemIdentity> {
        for _ in 0..3 {
            let admission = self.snapshot()?;
            let binding = admission
                .current
                .filesystem_identity(self.root_identity, &self.test_root)?;
            if self.is_current(&admission)? {
                return Ok(binding);
            }
        }
        Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "managed library admission changed during binding observation",
        ))
    }
}

impl ManagedLibraryWitness {
    pub fn try_acquire(&self) -> io::Result<ManagedLibraryOperation> {
        self.authority
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "managed library expired"))?
            .try_acquire()
    }

    pub fn prepare_admission_rebind(
        &self,
        candidate: AdmittedAbsoluteDirectory,
    ) -> io::Result<PreparedManagedLibraryAdmissionRebind> {
        self.authority
            .upgrade()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "managed library expired"))?
            .prepare_admission_rebind(candidate)
    }
}

impl ManagedLibraryOperation {
    pub fn witness(&self) -> ManagedLibraryWitness {
        ManagedLibraryWitness {
            authority: Arc::downgrade(&self.authority),
        }
    }

    pub fn prepare_layout(&self) -> io::Result<()> {
        let root = self.managed_directory().map_err(loader_io)?;
        for name in ["versions", "libraries", "assets"] {
            root.open_or_create_child(name).map_err(loader_io)?;
        }
        root.open_or_create_child("cache")
            .and_then(|cache| cache.open_or_create_child("loaders"))
            .and_then(|loaders| loaders.open_or_create_child("catalog"))
            .map_err(loader_io)?;
        root.sync().map_err(loader_io)?;
        self.revalidate()
    }

    pub fn revalidate(&self) -> io::Result<()> {
        self.authority.revalidate()
    }

    pub(crate) fn managed_directory(&self) -> Result<ManagedDir, LoaderError> {
        self.revalidate().map_err(LoaderError::Io)?;
        let directory = self
            .authority
            .root
            .with_operation_pin(Arc::clone(&self.pin));
        directory.revalidate()?;
        Ok(directory)
    }

    pub(crate) fn owns_directory(&self, directory: &ManagedDir) -> bool {
        self.authority.root.shares_root(directory)
            && directory.inner.operation_pin.is_some()
    }
}

impl ManagedLibraryRetirement {
    pub async fn drain_and_settle(&self) -> io::Result<ManagedLibraryRetirementBinding> {
        self.authority.lifecycle.drain().await?;
        let admission = self.authority.admission_snapshot()?;
        let exact_root = self
            .authority
            .root
            .settle()
            .and_then(|()| self.authority.root.revalidate());
        match exact_root {
            Ok(()) => {
                let binding = if admission
                    .current
                    .revalidate(
                        self.authority.admission.root_identity,
                        &self.authority.admission.test_root,
                    )
                    .is_ok()
                {
                    ManagedLibraryRetirementBinding::BindingIntact
                } else {
                    ManagedLibraryRetirementBinding::BindingLost
                };
                self.authority.root.revalidate().map_err(loader_io)?;
                Ok(binding)
            }
            Err(error) => {
                self.authority.root.inner.root.require_settled()?;
                if admission
                    .current
                    .revalidate(
                        self.authority.admission.root_identity,
                        &self.authority.admission.test_root,
                    )
                    .is_ok()
                {
                    return Err(loader_io(error));
                }
                self.authority
                    .root
                    .inner
                    .root
                    .validate_retained_authority()?;
                Ok(ManagedLibraryRetirementBinding::BindingLost)
            }
        }
    }
}

impl PreparedManagedLibraryAdmissionRebind {
    pub fn commit(mut self) -> Result<(), ManagedLibraryAdmissionRebindFailure> {
        let candidate = self
            .candidate
            .take()
            .expect("prepared library admission retains its candidate");
        let authority = &self.operation.authority;
        let observed_epoch = match authority.admission.state.read() {
            Ok(admission) => admission.epoch,
            Err(_) => std::process::abort(),
        };
        if observed_epoch != self.expected_epoch {
            return Err(ManagedLibraryAdmissionRebindFailure::Stale(candidate));
        }
        let root = &authority.root;
        if root.inner.root.validate_retained_authority().is_err()
            || candidate.revalidate().is_err()
            || candidate.filesystem_identity().ok()
                != Some(root.inner.identity.filesystem_identity())
            || root.inner.root.validate_retained_authority().is_err()
        {
            return Err(ManagedLibraryAdmissionRebindFailure::BindingLost(
                candidate,
            ));
        }
        let lifecycle = match authority.lifecycle.state.lock() {
            Ok(lifecycle) => lifecycle,
            Err(_) => std::process::abort(),
        };
        if !lifecycle.open {
            return Err(ManagedLibraryAdmissionRebindFailure::GenerationClosed(
                candidate,
            ));
        }
        let mut admission = match authority.admission.state.write() {
            Ok(admission) => admission,
            Err(_) => std::process::abort(),
        };
        if admission.epoch != self.expected_epoch {
            return Err(ManagedLibraryAdmissionRebindFailure::Stale(candidate));
        }
        if !matches!(&admission.current, ManagedLibraryAdmission::App(_)) {
            std::process::abort();
        }
        let next_epoch = match admission.epoch.checked_add(1) {
            Some(epoch) => epoch,
            None => std::process::abort(),
        };
        let previous = std::mem::replace(
            &mut *admission,
            Arc::new(ManagedLibraryAdmissionState {
                current: ManagedLibraryAdmission::App(candidate),
                epoch: next_epoch,
            }),
        );
        drop(admission);
        drop(lifecycle);
        drop(previous);
        Ok(())
    }
}

impl ManagedLibraryAdmission {
    fn filesystem_identity(
        &self,
        root_identity: axial_fs::DirectoryFilesystemIdentity,
        test_root: &Weak<ManagedRoot>,
    ) -> io::Result<axial_fs::DirectoryFilesystemIdentity> {
        match self {
            Self::App(admission) => admission.filesystem_identity(),
            #[cfg(test)]
            Self::Test { .. } => {
                self.revalidate(root_identity, test_root)?;
                Ok(root_identity)
            }
        }
    }

    fn revalidate(
        &self,
        root_identity: axial_fs::DirectoryFilesystemIdentity,
        test_root: &Weak<ManagedRoot>,
    ) -> io::Result<()> {
        match self {
            Self::App(admission) => {
                if admission.filesystem_identity()? != root_identity {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "managed library lease no longer matches the admitted directory",
                    ));
                }
                Ok(())
            }
            #[cfg(test)]
            Self::Test { path } => test_root
                .upgrade()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "managed root expired"))?
                .validate_requested_binding(path)
                .map_err(loader_io),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ManagedTreeCopyLimits {
    pub max_depth: usize,
    pub max_entries: usize,
    pub max_bytes: u64,
}

#[derive(Debug)]
pub enum ManagedTreeCopyFailure {
    Io(io::Error),
    UnsupportedEntry,
    DepthLimit,
    EntryLimit,
    ByteLimit,
}

impl From<io::Error> for ManagedTreeCopyFailure {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl From<LoaderError> for ManagedTreeCopyFailure {
    fn from(error: LoaderError) -> Self {
        match error {
            LoaderError::Io(error) => Self::Io(error),
            error => Self::Io(io::Error::new(io::ErrorKind::InvalidData, error.to_string())),
        }
    }
}

#[derive(Debug)]
pub enum ManagedTreeCopyOutcome {
    Applied(PortableFileName),
    RefusedBeforeMove(ManagedTreeCopyFailure),
    CleanupRetained {
        cause: ManagedTreeCopyFailure,
        cleanup: io::Error,
    },
    Indeterminate(io::Error),
}

#[must_use = "managed tree roots must be retired so retained effects are settled"]
pub struct ManagedTreeRoot {
    authority: Arc<ManagedTreeAuthority>,
}

struct ManagedTreeAuthority {
    root: ManagedDir,
    lifecycle: Arc<ManagedAuthorityLifecycle>,
}

#[derive(Clone)]
pub struct ManagedTreeOperation {
    authority: Arc<ManagedTreeAuthority>,
    pin: Arc<ManagedOperationPin>,
}

#[must_use = "retiring managed tree authority must be drained and settled"]
pub struct ManagedTreeRetirement {
    authority: Arc<ManagedTreeAuthority>,
}

impl std::fmt::Debug for ManagedTreeRoot {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedTreeRoot")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedTreeOperation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedTreeOperation")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedTreeRetirement {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedTreeRetirement")
            .finish_non_exhaustive()
    }
}

impl Drop for ManagedTreeRoot {
    fn drop(&mut self) {
        self.authority.lifecycle.close();
    }
}

impl ManagedTreeRoot {
    pub fn from_directory(directory: Directory, effects: EffectOwner) -> io::Result<Self> {
        let root = ManagedDir::from_directory(directory, effects).map_err(loader_io)?;
        Self::finish_construction(root)
    }

    fn finish_construction(root: ManagedDir) -> io::Result<Self> {
        root.settle().map_err(loader_io)?;
        let managed = Self {
            authority: Arc::new(ManagedTreeAuthority {
                root,
                lifecycle: ManagedAuthorityLifecycle::new(),
            }),
        };
        managed.revalidate()?;
        Ok(managed)
    }

    #[cfg(test)]
    pub(crate) fn open_for_test(path: &Path) -> io::Result<Self> {
        let root = ManagedDir::open_root(path).map_err(loader_io)?;
        Self::finish_construction(root)
    }

    pub fn try_acquire(&self) -> io::Result<ManagedTreeOperation> {
        self.authority.try_acquire()
    }

    pub fn begin_retirement(self) -> ManagedTreeRetirement {
        self.authority.lifecycle.close();
        ManagedTreeRetirement {
            authority: Arc::clone(&self.authority),
        }
    }

    fn revalidate(&self) -> io::Result<()> {
        self.authority.revalidate()
    }
}

impl ManagedTreeAuthority {
    fn try_acquire(self: &Arc<Self>) -> io::Result<ManagedTreeOperation> {
        self.root.settle().map_err(loader_io)?;
        self.revalidate()?;
        let operation = ManagedTreeOperation {
            authority: Arc::clone(self),
            pin: self.lifecycle.acquire_pin(None)?,
        };
        operation.revalidate()?;
        Ok(operation)
    }

    fn revalidate(&self) -> io::Result<()> {
        self.root.revalidate().map_err(loader_io)
    }
}

impl ManagedTreeOperation {
    pub fn directory(&self) -> io::Result<ManagedTreeDirectory> {
        self.revalidate()?;
        let directory = self
            .authority
            .root
            .with_operation_pin(Arc::clone(&self.pin));
        directory.revalidate().map_err(loader_io)?;
        Ok(ManagedTreeDirectory { directory })
    }

    fn revalidate(&self) -> io::Result<()> {
        self.pin.verify_admission()?;
        self.authority.revalidate()?;
        self.pin.verify_admission()
    }
}

impl ManagedTreeRetirement {
    pub fn try_drain_and_settle(&self) -> io::Result<Option<()>> {
        if !self.authority.lifecycle.is_drained()? {
            return Ok(None);
        }
        self.settle_drained()?;
        Ok(Some(()))
    }

    pub async fn wait_for_drain(&self) -> io::Result<()> {
        self.authority.lifecycle.drain().await
    }

    pub fn settle_drained(&self) -> io::Result<()> {
        if !self.authority.lifecycle.is_drained()? {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "managed tree retirement still has active operations",
            ));
        }
        self.authority.root.settle().map_err(loader_io)
    }
}

#[derive(Clone)]
pub struct ManagedTreeDirectory {
    directory: ManagedDir,
}

impl std::fmt::Debug for ManagedTreeDirectory {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedTreeDirectory")
            .finish_non_exhaustive()
    }
}

struct ManagedTreeBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
    max_depth: usize,
}

impl ManagedTreeBudget {
    fn enter(&self, depth: usize) -> Result<(), ManagedTreeCopyFailure> {
        if depth > self.max_depth {
            Err(ManagedTreeCopyFailure::DepthLimit)
        } else {
            Ok(())
        }
    }

    fn reserve_entry(&mut self) -> Result<(), ManagedTreeCopyFailure> {
        self.remaining_entries = self
            .remaining_entries
            .checked_sub(1)
            .ok_or(ManagedTreeCopyFailure::EntryLimit)?;
        Ok(())
    }

    fn reserve_bytes(&mut self, bytes: u64) -> Result<(), ManagedTreeCopyFailure> {
        self.remaining_bytes = self
            .remaining_bytes
            .checked_sub(bytes)
            .ok_or(ManagedTreeCopyFailure::ByteLimit)?;
        Ok(())
    }
}

impl ManagedTreeDirectory {
    pub fn open_child(&self, name: &str) -> io::Result<Option<Self>> {
        PortableFileName::new_exact(name).map_err(|_| invalid_name())?;
        match self.directory.open_child(name) {
            Ok(directory) => Ok(Some(Self { directory })),
            Err(LoaderError::Io(error)) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(loader_io(error)),
        }
    }

    pub fn open_or_create_child(&self, name: &str) -> io::Result<Self> {
        PortableFileName::new_exact(name).map_err(|_| invalid_name())?;
        self.directory
            .open_or_create_child(name)
            .map(|directory| Self { directory })
            .map_err(loader_io)
    }

    pub fn copy_tree_no_replace(
        &self,
        source: &Self,
        final_names: &[PortableFileName],
        stage_names: &[PortableFileName],
        limits: ManagedTreeCopyLimits,
    ) -> ManagedTreeCopyOutcome {
        if final_names.is_empty()
            || stage_names.is_empty()
            || limits.max_entries == 0
            || limits.max_entries > MAX_MANAGED_TREE_OPERATION_ENTRIES
            || limits.max_depth > MAX_MANAGED_TREE_OPERATION_DEPTH
            || final_names.len() > MAX_MANAGED_TREE_NAME_CANDIDATES
            || stage_names.len() > MAX_MANAGED_TREE_NAME_CANDIDATES
        {
            return ManagedTreeCopyOutcome::RefusedBeforeMove(invalid_plan().into());
        }
        let final_keys = final_names
            .iter()
            .map(PortableFileName::key)
            .collect::<BTreeSet<_>>();
        let stage_keys = stage_names
            .iter()
            .map(PortableFileName::key)
            .collect::<BTreeSet<_>>();
        if final_keys.len() != final_names.len()
            || stage_keys.len() != stage_names.len()
            || !final_keys.is_disjoint(&stage_keys)
        {
            return ManagedTreeCopyOutcome::RefusedBeforeMove(invalid_plan().into());
        }
        if let Err(error) = self
            .directory
            .inner
            .root
            .settle()
            .and_then(|_| source.directory.revalidate())
            .and_then(|_| self.directory.revalidate())
        {
            return ManagedTreeCopyOutcome::RefusedBeforeMove(error.into());
        }
        let final_name = match choose_absent_name(&self.directory, final_names) {
            Ok(name) => name,
            Err(error) => return ManagedTreeCopyOutcome::RefusedBeforeMove(error.into()),
        };
        let (stage_name, stage) = match create_stage_directory(&self.directory, stage_names) {
            Ok(stage) => stage,
            Err(error) => return ManagedTreeCopyOutcome::RefusedBeforeMove(error.into()),
        };
        let source_revision = match source.directory.inner.directory.revision() {
            Ok(revision) => revision,
            Err(error) => return cleanup_tree_failure(&self.directory, &stage_name, stage, error.into()),
        };
        let mut budget = ManagedTreeBudget {
            remaining_entries: limits.max_entries,
            remaining_bytes: limits.max_bytes,
            max_depth: limits.max_depth,
        };
        if let Err(cause) = copy_tree_contents(&source.directory, &stage, 0, &mut budget) {
            return cleanup_tree_failure(&self.directory, &stage_name, stage, cause);
        }
        if source
            .directory
            .inner
            .directory
            .validate_revision(&source_revision)
            .is_err()
        {
            return cleanup_tree_failure(
                &self.directory,
                &stage_name,
                stage,
                world_source_revision_drift(),
            );
        }
        if let Err(error) = stage.sync() {
            return cleanup_tree_failure(&self.directory, &stage_name, stage, error.into());
        }
        let final_leaf = match leaf(final_name.as_str()) {
            Ok(name) => name,
            Err(error) => return cleanup_tree_failure(&self.directory, &stage_name, stage, error.into()),
        };
        let root = self.directory.inner.root.clone();
        let transition = root.transition();
        if let Err(error) = root
            .settle_locked(&transition)
            .and_then(|()| self.directory.revalidate_locked(&transition))
            .and_then(|()| stage.revalidate_locked(&transition))
        {
            drop(transition);
            return cleanup_tree_failure(&self.directory, &stage_name, stage, error.into());
        }
        match stage.inner.directory.clone().move_no_replace(
            &self.directory.inner.directory,
            &final_leaf,
        ) {
            DirectoryMoveOutcome::Applied(directory) => {
                drop(directory);
                ManagedTreeCopyOutcome::Applied(final_name)
            }
            DirectoryMoveOutcome::NoEffect { error, directory } => {
                drop(directory);
                drop(transition);
                cleanup_tree_failure(&self.directory, &stage_name, stage, error.into())
            }
            DirectoryMoveOutcome::AppliedUnverified(obligation) => {
                let receipt = root.retain_linear_locked(
                    &transition,
                    obligation,
                    EffectOwner::retain_directory_move,
                );
                let _settlement = root.effects.settle();
                match receipt.claim() {
                    DirectoryMoveReceiptOutcome::Applied(directory) => {
                        drop(directory);
                        if let Err(error) = root.settle_locked(&transition) {
                            return ManagedTreeCopyOutcome::Indeterminate(loader_io(error));
                        }
                        ManagedTreeCopyOutcome::Applied(final_name)
                    }
                    DirectoryMoveReceiptOutcome::NoEffect(directory) => {
                        drop(directory);
                        let cause = ManagedTreeCopyFailure::Io(io::Error::other(
                            "world backup publication had no effect",
                        ));
                        let cleanup = retain_tree_cleanup(
                            &transition,
                            ManagedDirDescriptor::capture(&self.directory),
                            stage_name.clone(),
                            ManagedDirDescriptor::capture(&stage),
                        );
                        match cleanup {
                            None => ManagedTreeCopyOutcome::RefusedBeforeMove(cause),
                            Some(cleanup) => {
                                root.retain_continuation_locked(&transition, cleanup);
                                ManagedTreeCopyOutcome::CleanupRetained {
                                    cause,
                                    cleanup: io::Error::new(
                                        io::ErrorKind::WouldBlock,
                                        "world backup cleanup remains retained",
                                    ),
                                }
                            }
                        }
                    }
                    DirectoryMoveReceiptOutcome::Pending(receipt) => {
                        root.retain_continuation_locked(
                            &transition,
                            ManagedEffectContinuation::TreeDirectoryMove {
                                receipt,
                                parent: ManagedDirDescriptor::capture(&self.directory),
                                stage_name,
                                stage: ManagedDirDescriptor::capture(&stage),
                            },
                        );
                        ManagedTreeCopyOutcome::Indeterminate(io::Error::other(
                            "world backup publication remains unsettled",
                        ))
                    }
                }
            }
        }
    }
}

fn world_source_revision_drift() -> ManagedTreeCopyFailure {
    ManagedTreeCopyFailure::Io(io::Error::new(
        io::ErrorKind::WouldBlock,
        "world source changed during backup",
    ))
}

fn choose_absent_name(
    directory: &ManagedDir,
    names: &[PortableFileName],
) -> Result<PortableFileName, LoaderError> {
    for name in names {
        if !directory.has_portably_exact_child_name(name.as_str())? {
            return Ok(name.clone());
        }
    }
    Err(LoaderError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "managed tree has no absent destination name",
    )))
}

fn create_stage_directory(
    parent: &ManagedDir,
    names: &[PortableFileName],
) -> Result<(PortableFileName, ManagedDir), LoaderError> {
    for name in names {
        if parent.has_portably_exact_child_name(name.as_str())? {
            continue;
        }
        match parent.create_child_new(name.as_str()) {
            Ok(directory) => return Ok((name.clone(), directory)),
            Err(LoaderError::Io(error)) if error.kind() == io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error),
        }
    }
    Err(LoaderError::Io(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "managed tree has no absent stage name",
    )))
}

fn copy_tree_contents(
    source: &ManagedDir,
    target: &ManagedDir,
    depth: usize,
    budget: &mut ManagedTreeBudget,
) -> Result<(), ManagedTreeCopyFailure> {
    budget.enter(depth)?;
    let source_revision = source.inner.directory.revision()?;
    let entries = source.listing(MAX_MANAGED_TREE_OPERATION_ENTRIES)?;
    for entry in entries {
        budget.reserve_entry()?;
        let name = entry
            .utf8_name()
            .ok_or(ManagedTreeCopyFailure::UnsupportedEntry)?;
        PortableFileName::new_exact(name).map_err(|_| ManagedTreeCopyFailure::UnsupportedEntry)?;
        match entry.kind() {
            EntryKind::File => {
                let guard = source
                    .inspect_regular_file(name)?
                    .ok_or(ManagedTreeCopyFailure::UnsupportedEntry)?;
                budget.reserve_bytes(guard.size())?;
                let bytes = source.read_guarded_file_bounded(name, &guard, guard.size())?;
                target.write_new_exact(name, &bytes)?;
            }
            EntryKind::Directory => {
                let child_source = source.open_observed_child(&entry)?;
                let child_target = target.create_child_new(name)?;
                copy_tree_contents(&child_source, &child_target, depth + 1, budget)?;
                child_target.sync()?;
            }
            EntryKind::Link | EntryKind::Other => {
                return Err(ManagedTreeCopyFailure::UnsupportedEntry);
            }
        }
    }
    source
        .inner
        .directory
        .validate_revision(&source_revision)
        .map_err(ManagedTreeCopyFailure::Io)?;
    Ok(())
}

fn cleanup_tree_failure(
    parent: &ManagedDir,
    stage_name: &PortableFileName,
    stage: ManagedDir,
    cause: ManagedTreeCopyFailure,
) -> ManagedTreeCopyOutcome {
    let root = parent.inner.root.clone();
    let transition = root.transition();
    let parent = ManagedDirDescriptor::capture(parent);
    let stage = ManagedDirDescriptor::capture(&stage);
    let cleanup = if root.settle_locked(&transition).is_ok() {
        retain_tree_cleanup(&transition, parent, stage_name.clone(), stage)
    } else {
        Some(ManagedEffectContinuation::TreeCleanup {
            parent,
            stage_name: stage_name.clone(),
            stage,
        })
    };
    match cleanup {
        None => ManagedTreeCopyOutcome::RefusedBeforeMove(cause),
        Some(cleanup) => {
            root.retain_continuation_locked(&transition, cleanup);
            ManagedTreeCopyOutcome::CleanupRetained {
                cause,
                cleanup: io::Error::new(
                    io::ErrorKind::WouldBlock,
                    format!("{} cleanup remains retained", stage_name.as_str()),
                ),
            }
        }
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ManagedTreeLimits {
    max_entries: usize,
    max_depth: usize,
    max_file_bytes: u64,
    max_total_bytes: u64,
}

pub(crate) struct ManagedTreeUsage {
    entries: usize,
    bytes: u64,
}

impl ManagedTreeUsage {
    pub(crate) fn entries(&self) -> usize {
        self.entries
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
}

impl ManagedTreeLimits {
    pub(crate) fn processor_stage() -> Self {
        Self {
            max_entries: MAX_MANAGED_TREE_ENTRIES,
            max_depth: MAX_MANAGED_TREE_DEPTH,
            max_file_bytes: MAX_MANAGED_TREE_FILE_BYTES,
            max_total_bytes: MAX_MANAGED_TREE_TOTAL_BYTES,
        }
    }

    #[cfg(test)]
    fn bounded_test(
        max_entries: usize,
        max_depth: usize,
        max_file_bytes: u64,
        max_total_bytes: u64,
    ) -> Self {
        assert!(max_entries <= MAX_MANAGED_TREE_ENTRIES);
        assert!(max_depth <= MAX_MANAGED_TREE_DEPTH);
        assert!(max_file_bytes <= MAX_MANAGED_TREE_FILE_BYTES);
        assert!(max_total_bytes <= MAX_MANAGED_TREE_TOTAL_BYTES);
        Self {
            max_entries,
            max_depth,
            max_file_bytes,
            max_total_bytes,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct ManagedFileFact {
    size: u64,
    sha1: [u8; 20],
}

impl ManagedFileFact {
    pub(crate) fn size(&self) -> u64 {
        self.size
    }

    pub(crate) fn sha1(&self) -> &[u8; 20] {
        &self.sha1
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedTreeSnapshot {
    files: BTreeMap<PortableRelativePath, ManagedFileFact>,
    directories: BTreeSet<PortableRelativePath>,
}

impl ManagedTreeSnapshot {
    pub(crate) fn files(&self) -> &BTreeMap<PortableRelativePath, ManagedFileFact> {
        &self.files
    }

    pub(crate) fn directories(&self) -> &BTreeSet<PortableRelativePath> {
        &self.directories
    }

    pub(crate) fn diff(&self, after: &Self) -> ManagedTreeDiff {
        let added_files = after
            .files
            .iter()
            .filter(|(path, _)| !self.files.contains_key(*path))
            .map(|(path, fact)| (path.clone(), fact.clone()))
            .collect();
        let removed_files = self
            .files
            .iter()
            .filter(|(path, _)| !after.files.contains_key(*path))
            .map(|(path, fact)| (path.clone(), fact.clone()))
            .collect();
        let modified_files = self
            .files
            .iter()
            .filter_map(|(path, before)| {
                after
                    .files
                    .get(path)
                    .filter(|after| *after != before)
                    .map(|after| (path.clone(), (before.clone(), after.clone())))
            })
            .collect();
        ManagedTreeDiff {
            added_files,
            removed_files,
            modified_files,
            added_directories: after
                .directories
                .difference(&self.directories)
                .cloned()
                .collect(),
            removed_directories: self
                .directories
                .difference(&after.directories)
                .cloned()
                .collect(),
        }
    }
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagedTreeDiff {
    added_files: BTreeMap<PortableRelativePath, ManagedFileFact>,
    removed_files: BTreeMap<PortableRelativePath, ManagedFileFact>,
    modified_files: BTreeMap<PortableRelativePath, (ManagedFileFact, ManagedFileFact)>,
    added_directories: BTreeSet<PortableRelativePath>,
    removed_directories: BTreeSet<PortableRelativePath>,
}

impl ManagedTreeDiff {
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.added_files.is_empty()
            && self.removed_files.is_empty()
            && self.modified_files.is_empty()
            && self.added_directories.is_empty()
            && self.removed_directories.is_empty()
    }

    pub(crate) fn added_files(&self) -> &BTreeMap<PortableRelativePath, ManagedFileFact> {
        &self.added_files
    }

    pub(crate) fn removed_files(&self) -> &BTreeMap<PortableRelativePath, ManagedFileFact> {
        &self.removed_files
    }

    pub(crate) fn modified_files(
        &self,
    ) -> &BTreeMap<PortableRelativePath, (ManagedFileFact, ManagedFileFact)> {
        &self.modified_files
    }

    pub(crate) fn added_directories(&self) -> &BTreeSet<PortableRelativePath> {
        &self.added_directories
    }

    pub(crate) fn removed_directories(&self) -> &BTreeSet<PortableRelativePath> {
        &self.removed_directories
    }
}

fn invalid_name() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "managed leaf name is invalid")
}

fn invalid_plan() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "managed tree copy plan is invalid")
}

fn loader_io(error: LoaderError) -> io::Error {
    match error {
        LoaderError::Io(error) => error,
        error => io::Error::new(io::ErrorKind::InvalidData, error.to_string()),
    }
}

fn leaf(name: &str) -> Result<LeafName, LoaderError> {
    PortableFileName::new_exact(name)
        .map_err(|_| LoaderError::Verify("managed leaf name is not portable".to_string()))?;
    LeafName::new(name).map_err(|_| LoaderError::Verify("managed leaf name is invalid".to_string()))
}

fn portable_key(name: &str) -> Result<PortablePathKey, LoaderError> {
    PortableFileName::new_exact(name)
        .map(|name| name.key())
        .map_err(|_| LoaderError::Verify("managed leaf name is not portable".to_string()))
}

#[cfg(test)]
mod library_lifecycle_tests {
    use super::*;
    use std::time::Duration;

    fn managed_library(prefix: &str) -> (tempfile::TempDir, ManagedLibraryRoot) {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-managed-library-{prefix}-"))
            .tempdir()
            .expect("temporary managed library");
        let root = ManagedLibraryRoot::open_for_test(temporary.path())
            .expect("open managed library root");
        (temporary, root)
    }

    #[test]
    fn close_and_owner_drop_reject_new_operations_and_witnesses() {
        let (_temporary, root) = managed_library("close");
        let witness = root.witness();
        let retained = root.try_acquire().expect("initial operation");
        let _retirement = root.begin_retirement();
        assert!(witness.try_acquire().is_err());
        drop(retained);

        let (_temporary, root) = managed_library("drop");
        let witness = root.witness();
        drop(root);
        assert!(witness.try_acquire().is_err());
    }

    #[test]
    fn library_root_owns_the_effect_owner_for_its_admitted_binding() {
        let (_temporary, root) = managed_library("owner-binding");
        assert!(
            root.authority
                .root
                .inner
                .root
                .effects
                .anchor_identity()
                .same_filesystem_object(
                    root.authority.root.inner.identity
                )
        );
        assert_eq!(
            root.binding().expect("root binding"),
            ManagedLibraryBinding(root.authority.root.inner.identity.filesystem_identity())
        );
    }

    #[test]
    fn layout_preparation_creates_exact_children_and_refuses_aliases() {
        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-layout-")
            .tempdir()
            .expect("temporary library");
        let root = ManagedLibraryRoot::open_for_test(temporary.path())
            .expect("managed library root");
        let operation = root.try_acquire().expect("managed library operation");
        operation.prepare_layout().expect("prepare managed layout");
        for relative in [
            "versions",
            "libraries",
            "assets",
            "cache/loaders/catalog",
        ] {
            assert!(temporary.path().join(relative).is_dir(), "missing {relative}");
        }
        drop((operation, root));

        let aliased = tempfile::Builder::new()
            .prefix("axial-managed-library-layout-alias-")
            .tempdir()
            .expect("temporary aliased library");
        std::fs::create_dir(aliased.path().join("Versions")).expect("version alias");
        let root = ManagedLibraryRoot::open_for_test(aliased.path())
            .expect("aliased managed library root");
        let operation = root.try_acquire().expect("aliased library operation");
        assert!(operation.prepare_layout().is_err());
        assert!(!aliased.path().join("versions").exists());
    }

    #[tokio::test]
    async fn one_physical_root_cannot_open_an_independent_library_generation() {
        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-alias-")
            .tempdir()
            .expect("temporary library");
        let library = temporary.path().join("library");
        let first_app = temporary.path().join("first-app-root");
        let second_app = temporary.path().join("second-app-root");
        std::fs::create_dir(&library).expect("library root");
        let first_app_session = acquire_root_session(&first_app).expect("first app session");
        let first_admission = first_app_session
            .admit_absolute_directory_authority(&library)
            .expect("first admission");
        let second_app_session = acquire_root_session(&second_app).expect("second app session");
        let second_admission = second_app_session
            .admit_absolute_directory_authority(&library)
            .expect("second admission");
        assert_eq!(
            ManagedLibraryRoot::admitted_binding(&first_admission)
                .expect("first binding"),
            ManagedLibraryRoot::admitted_binding(&second_admission)
                .expect("second binding")
        );
        let first = ManagedLibraryRoot::from_admitted_directory(first_admission)
            .expect("first generation");
        assert!(ManagedLibraryRoot::from_admitted_directory(second_admission).is_err());
        assert!(acquire_root_session(&library).is_err());
        let operation = first.try_acquire().expect("active operation");
        let retirement = first.begin_retirement();
        assert!(acquire_root_session(&library).is_err());
        drop(operation);
        retirement
            .drain_and_settle()
            .await
            .expect("retirement settles");
        assert!(acquire_root_session(&library).is_err());
        drop(retirement);
        let reacquired =
            acquire_root_session(&library).expect("lease released after retirement");
        assert!(matches!(
            reacquired.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
        drop((first_app_session, second_app_session));
    }

    #[tokio::test]
    async fn admission_rebind_is_send_static_and_rejects_a_stale_prepare() {
        fn assert_send_static<T: Send + 'static>(_: &T) {}

        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-rebind-")
            .tempdir()
            .expect("temporary parent");
        let library = temporary.path().join("library");
        let app = temporary.path().join("app-root");
        std::fs::create_dir(&library).expect("library root");
        let app_session = acquire_root_session(&app).expect("app session");
        let initial = app_session
            .admit_absolute_directory_authority(&library)
            .expect("initial admission");
        let first_candidate = app_session
            .admit_absolute_directory_authority(&library)
            .expect("first candidate");
        let second_candidate = app_session
            .admit_absolute_directory_authority(&library)
            .expect("second candidate");
        let root = ManagedLibraryRoot::from_admitted_directory(initial)
            .expect("managed library");
        let witness = root.witness();
        let first = witness
            .prepare_admission_rebind(first_candidate)
            .expect("first prepare");
        let second = witness
            .prepare_admission_rebind(second_candidate)
            .expect("second prepare");
        assert_send_static(&first);
        let first = std::thread::spawn(move || first)
            .join()
            .expect("prepared carrier thread");
        first.commit().expect("first commit");
        let retained = match second.commit() {
            Err(ManagedLibraryAdmissionRebindFailure::Stale(candidate)) => candidate,
            outcome => panic!("second commit had unexpected outcome: {outcome:?}"),
        };
        retained.revalidate().expect("stale candidate remains valid");
        let retirement = root.begin_retirement();
        assert_eq!(
            retirement
                .drain_and_settle()
                .await
                .expect("retirement settles"),
            ManagedLibraryRetirementBinding::BindingIntact
        );
        drop((retirement, retained, app_session));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn clean_retirement_reports_lost_binding_without_touching_replacement() {
        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-binding-loss-")
            .tempdir()
            .expect("temporary parent");
        let library = temporary.path().join("library");
        let displaced = temporary.path().join("displaced-library");
        let app = temporary.path().join("app-root");
        std::fs::create_dir(&library).expect("library root");
        let app_session = acquire_root_session(&app).expect("app session");
        let admission = app_session
            .admit_absolute_directory_authority(&library)
            .expect("library admission");
        let candidate = app_session
            .admit_absolute_directory_authority(&library)
            .expect("rebind candidate");
        let root = ManagedLibraryRoot::from_admitted_directory(admission)
            .expect("managed library");
        let prepared = root
            .prepare_admission_rebind(candidate)
            .expect("prepare rebind");
        std::fs::rename(&library, &displaced).expect("displace library");
        std::fs::create_dir(&library).expect("replacement library");

        let lost_candidate = match prepared.commit() {
            Err(ManagedLibraryAdmissionRebindFailure::BindingLost(candidate)) => candidate,
            outcome => panic!("binding-loss commit had unexpected outcome: {outcome:?}"),
        };
        drop(lost_candidate);

        let retirement = root.begin_retirement();
        assert_eq!(
            retirement
                .drain_and_settle()
                .await
                .expect("clean retirement settles"),
            ManagedLibraryRetirementBinding::BindingLost
        );
        assert!(
            !library.join(ROOT_LEASE_NAME).exists(),
            "retirement touched replacement"
        );
        assert!(acquire_root_session(&displaced).is_err());
        let replacement = acquire_root_session(&library).expect("replacement is independent");
        assert!(matches!(
            replacement.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
        drop(retirement);
        let released = acquire_root_session(&displaced).expect("displaced lease released");
        assert!(matches!(
            released.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
        drop(app_session);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn stale_admission_can_heal_to_the_same_retained_physical_root() {
        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-heal-")
            .tempdir()
            .expect("temporary parent");
        let library = temporary.path().join("library");
        let displaced = temporary.path().join("displaced-library");
        let app = temporary.path().join("app-root");
        std::fs::create_dir(&library).expect("library root");
        let app_session = acquire_root_session(&app).expect("app session");
        let initial = app_session
            .admit_absolute_directory_authority(&library)
            .expect("initial admission");
        let root = ManagedLibraryRoot::from_admitted_directory(initial)
            .expect("managed library");
        let witness = root.witness();
        let operation = root.try_acquire().expect("initial operation");
        let directory = operation.managed_directory().expect("managed directory");

        std::fs::rename(&library, &displaced).expect("displace library");
        assert!(witness.try_acquire().is_err());
        assert!(directory.revalidate().is_err());
        assert!(acquire_root_session(&displaced).is_err());

        let candidate = app_session
            .admit_absolute_directory_authority(&displaced)
            .expect("fresh same-physical admission");
        let prepared = witness
            .prepare_admission_rebind(candidate)
            .expect("prepare from stale admission");
        prepared.commit().expect("commit healed admission");

        directory.revalidate().expect("existing pin observes healing");
        directory
            .open_or_create_child("versions")
            .expect("managed operation after healing");
        witness
            .try_acquire()
            .expect("new operation after healing");
        assert!(acquire_root_session(&displaced).is_err());

        drop((directory, operation));
        let retirement = root.begin_retirement();
        assert_eq!(
            retirement
                .drain_and_settle()
                .await
                .expect("healed retirement settles"),
            ManagedLibraryRetirementBinding::BindingIntact
        );
        drop(retirement);
        let released = acquire_root_session(&displaced).expect("single lease was released");
        assert!(matches!(
            released.revoke(),
            axial_fs::RootRevokeOutcome::Revoked
        ));
        drop(app_session);
    }

    #[tokio::test]
    async fn prepared_rebind_rejects_a_closed_generation_and_retains_candidate() {
        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-library-closed-rebind-")
            .tempdir()
            .expect("temporary parent");
        let library = temporary.path().join("library");
        let app = temporary.path().join("app-root");
        std::fs::create_dir(&library).expect("library root");
        let app_session = acquire_root_session(&app).expect("app session");
        let initial = app_session
            .admit_absolute_directory_authority(&library)
            .expect("initial admission");
        let candidate = app_session
            .admit_absolute_directory_authority(&library)
            .expect("candidate admission");
        let root = ManagedLibraryRoot::from_admitted_directory(initial)
            .expect("managed library");
        let prepared = root
            .prepare_admission_rebind(candidate)
            .expect("prepared admission");
        let retirement = root.begin_retirement();
        let candidate = match prepared.commit() {
            Err(ManagedLibraryAdmissionRebindFailure::GenerationClosed(candidate)) => candidate,
            outcome => panic!("closed-generation commit had unexpected outcome: {outcome:?}"),
        };
        candidate.revalidate().expect("failure retained candidate");
        drop(candidate);
        assert_eq!(
            retirement
                .drain_and_settle()
                .await
                .expect("retirement settles"),
            ManagedLibraryRetirementBinding::BindingIntact
        );
        drop((retirement, app_session));
    }

    #[tokio::test]
    async fn derived_reader_and_identity_pin_retirement_until_release() {
        let (_temporary, root) = managed_library("derived-pin");
        let operation = root.try_acquire().expect("operation");
        let directory = operation.managed_directory().expect("managed directory");
        directory
            .write_new_exact("source.bin", b"source")
            .expect("write source");
        let guard = directory
            .inspect_regular_file("source.bin")
            .expect("inspect source")
            .expect("source guard");
        let identity = guard.identity();
        let reader = guard.into_bounded_reader(6).expect("bounded reader");
        let retirement = root.begin_retirement();
        let mut drain = tokio::spawn(async move { retirement.drain_and_settle().await });
        drop(directory);
        drop(operation);
        assert!(tokio::time::timeout(Duration::from_millis(25), &mut drain)
            .await
            .is_err());
        reader.cancel();
        assert!(tokio::time::timeout(Duration::from_millis(25), &mut drain)
            .await
            .is_err());
        drop(identity);
        drain
            .await
            .expect("retirement task")
            .expect("settled retirement");
    }

    #[tokio::test]
    async fn cancelled_retirement_drain_can_be_resumed() {
        let (_temporary, root) = managed_library("retry-drain");
        let operation = root.try_acquire().expect("operation");
        let retirement = root.begin_retirement();
        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                retirement.drain_and_settle(),
            )
            .await
            .is_err()
        );
        drop(operation);
        retirement
            .drain_and_settle()
            .await
            .expect("retry settles retirement");
    }

    #[tokio::test]
    async fn pinless_tree_cleanup_continuation_does_not_deadlock_retirement() {
        let (_temporary, root) = managed_library("tree-continuation");
        let operation = root.try_acquire().expect("operation");
        let directory = operation.managed_directory().expect("managed directory");
        let stage = directory.create_child_new("stage").expect("create stage");
        let owner = directory.inner.root.clone();
        {
            let transition = owner.transition();
            owner.retain_continuation_locked(
                &transition,
                ManagedEffectContinuation::TreeCleanup {
                    parent: ManagedDirDescriptor::capture(&directory),
                    stage_name: PortableFileName::new_exact("stage").expect("stage name"),
                    stage: ManagedDirDescriptor::capture(&stage),
                },
            );
        }
        let retirement = root.begin_retirement();
        drop(stage);
        drop(directory);
        drop(operation);
        tokio::time::timeout(Duration::from_secs(1), retirement.drain_and_settle())
            .await
            .expect("retirement did not deadlock")
            .expect("tree cleanup settled");
    }

    #[test]
    fn file_move_continuation_payload_is_pinless() {
        let (_temporary, root) = managed_library("file-move-payload");
        let operation = root.try_acquire().expect("operation");
        let directory = operation.managed_directory().expect("managed directory");
        directory
            .write_new_exact("source.bin", b"source")
            .expect("write source");
        let identity = directory
            .inspect_regular_file("source.bin")
            .expect("inspect source")
            .expect("source guard")
            .identity();
        let continuation_payload: Arc<ManagedFileProof> = identity.pinless_proof();
        drop(identity);
        drop(directory);
        drop(operation);
        assert_eq!(
            root.authority
                .lifecycle
                .state
                .lock()
                .expect("lifecycle")
                .active,
            0
        );
        drop(continuation_payload);
    }
}

#[cfg(test)]
mod managed_tree_lifecycle_tests {
    use super::*;
    use std::time::Duration;

    fn managed_tree(
        prefix: &str,
    ) -> (tempfile::TempDir, RootSession, PathBuf, ManagedTreeRoot) {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-managed-tree-{prefix}-"))
            .tempdir()
            .expect("temporary managed tree parent");
        let authority_path = temporary.path().join("authority");
        let tree_path = authority_path.join("tree");
        std::fs::create_dir_all(&tree_path).expect("managed tree directory");
        let session = acquire_root_session(&authority_path).expect("parent root session");
        let parent = session.root().expect("parent directory");
        let tree = parent
            .open_directory(&LeafName::new("tree").expect("tree leaf"))
            .expect("bound tree directory");
        let effects = tree.create_effect_owner().expect("tree effect owner");
        let root = ManagedTreeRoot::from_directory(tree, effects).expect("managed tree root");
        (temporary, session, tree_path, root)
    }

    #[test]
    fn child_capabilities_pin_retirement_and_closed_root_refuses_acquisition() {
        let (_temporary, _session, _tree_path, root) = managed_tree("child-pin");
        let authority = Arc::clone(&root.authority);
        let operation = root.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("operation directory");
        let child = directory
            .open_or_create_child("child")
            .expect("managed child");
        drop(directory);
        drop(operation);

        let retirement = root.begin_retirement();
        assert!(authority.try_acquire().is_err());
        assert!(retirement
            .try_drain_and_settle()
            .expect("retirement probe")
            .is_none());
        drop(child);
        assert_eq!(
            retirement
                .try_drain_and_settle()
                .expect("settled retirement"),
            Some(())
        );
    }

    #[tokio::test]
    async fn async_retirement_waits_for_derived_directory_and_can_resume() {
        let (_temporary, _session, _tree_path, root) = managed_tree("async-drain");
        let operation = root.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("operation directory");
        let child = directory
            .open_or_create_child("child")
            .expect("managed child");
        let retirement = root.begin_retirement();
        assert!(tokio::time::timeout(
            Duration::from_millis(25),
            retirement.wait_for_drain(),
        )
        .await
        .is_err());
        drop((child, directory, operation));
        tokio::time::timeout(Duration::from_secs(1), retirement.wait_for_drain())
            .await
            .expect("retirement drain did not resume")
            .expect("retirement drained");
        retirement.settle_drained().expect("retirement settled");
    }

    #[test]
    fn operation_revalidates_the_retained_parent_name_binding() {
        let (temporary, _session, tree_path, root) = managed_tree("parent-binding");
        let operation = root.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("operation directory");
        let displaced = temporary.path().join("displaced-tree");
        std::fs::rename(&tree_path, &displaced).expect("displace bound tree");
        std::fs::create_dir(&tree_path).expect("replace bound tree");
        std::fs::write(tree_path.join("replacement-marker"), b"replacement")
            .expect("write replacement marker");
        assert!(operation.revalidate().is_err());
        assert!(directory.open_or_create_child("must-not-appear").is_err());
        drop((directory, operation));
        let retirement = root.begin_retirement();
        assert!(retirement.try_drain_and_settle().is_err());
        assert_eq!(
            std::fs::read(tree_path.join("replacement-marker"))
                .expect("replacement remains readable"),
            b"replacement",
            "retirement must not settle effects against a lexical replacement",
        );
        assert!(!tree_path.join("must-not-appear").exists());
    }

    #[test]
    fn retirement_settles_retained_tree_cleanup() {
        let (_temporary, _session, tree_path, root) = managed_tree("settlement");
        let operation = root.try_acquire().expect("tree operation");
        let directory = operation.directory().expect("operation directory");
        let stage = directory
            .directory
            .create_child_new("stage")
            .expect("create stage");
        let managed_root = directory.directory.inner.root.clone();
        {
            let transition = managed_root.transition();
            managed_root.retain_continuation_locked(
                &transition,
                ManagedEffectContinuation::TreeCleanup {
                    parent: ManagedDirDescriptor::capture(&directory.directory),
                    stage_name: PortableFileName::new_exact("stage").expect("stage name"),
                    stage: ManagedDirDescriptor::capture(&stage),
                },
            );
        }
        let retirement = root.begin_retirement();
        drop((stage, directory, operation));
        assert_eq!(
            retirement
                .try_drain_and_settle()
                .expect("retained cleanup settles"),
            Some(())
        );
        assert!(!tree_path.join("stage").exists());
        managed_root.require_settled().expect("managed root settled");
    }

    #[test]
    fn new_acquisition_recovers_retained_tree_cleanup() {
        let (_temporary, _session, tree_path, root) = managed_tree("acquire-recovery");
        let directory = &root.authority.root;
        let stage = directory
            .create_child_new("stage")
            .expect("create stage");
        let managed_root = directory.inner.root.clone();
        {
            let transition = managed_root.transition();
            managed_root.retain_continuation_locked(
                &transition,
                ManagedEffectContinuation::TreeCleanup {
                    parent: ManagedDirDescriptor::capture(directory),
                    stage_name: PortableFileName::new_exact("stage").expect("stage name"),
                    stage: ManagedDirDescriptor::capture(&stage),
                },
            );
        }
        drop(stage);

        let operation = root
            .try_acquire()
            .expect("new acquisition recovers retained cleanup");
        assert!(!tree_path.join("stage").exists());
        managed_root.require_settled().expect("managed root settled");
        drop(operation);
        root.begin_retirement()
            .try_drain_and_settle()
            .expect("retirement settles")
            .expect("retirement is drained");
    }

    #[test]
    fn source_revision_drift_is_a_retryable_conflict() {
        let ManagedTreeCopyFailure::Io(error) = world_source_revision_drift() else {
            panic!("source revision drift was not an I/O failure");
        };
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }
}

#[cfg(test)]
mod effect_transition_tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    fn managed_test_root(prefix: &str) -> (tempfile::TempDir, ManagedDir) {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-managed-fs-{prefix}-"))
            .tempdir()
            .expect("temporary managed root");
        let root = ManagedDir::open_root(temporary.path()).expect("open managed root");
        (temporary, root)
    }

    #[test]
    fn effect_transition_blocks_false_clean_observation() {
        let (_temporary, directory) = managed_test_root("false-clean");
        let transition_root = directory.inner.root.clone();
        let observing_root = transition_root.clone();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _transition = transition_root.transition();
            entered_tx.send(()).expect("publish transition entry");
            release_rx.recv().expect("release transition");
        });
        entered_rx.recv().expect("transition entered");

        let (observing_tx, observing_rx) = mpsc::channel();
        let (observed_tx, observed_rx) = mpsc::channel();
        let observer = std::thread::spawn(move || {
            observing_tx.send(()).expect("publish observer start");
            observed_tx
                .send(observing_root.require_settled())
                .expect("publish settlement observation");
        });
        observing_rx.recv().expect("observer started");
        assert!(
            observed_rx.recv_timeout(Duration::from_millis(25)).is_err(),
            "require_settled observed a false-clean transition window"
        );
        release_tx.send(()).expect("release transition holder");
        assert!(
            observed_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("settlement observation")
                .is_ok()
        );
        holder.join().expect("transition holder");
        observer.join().expect("transition observer");
    }

    #[test]
    fn tree_cleanup_retry_removes_only_the_exact_stage_binding() {
        let (_temporary, root) = managed_test_root("tree-cleanup");
        let parent = root.create_child_new("parent").expect("create parent");
        let stage = parent.create_child_new("stage").expect("create stage");
        stage
            .write_new_exact("payload.bin", b"payload")
            .expect("write stage payload");
        let root_owner = root.inner.root.clone();
        let transition = root_owner.transition();
        let cleanup = retain_tree_cleanup(
            &transition,
            ManagedDirDescriptor::capture(&parent),
            PortableFileName::new_exact("stage").expect("stage name"),
            ManagedDirDescriptor::capture(&stage),
        );
        assert!(cleanup.is_none(), "exact stage cleanup did not settle");
        drop(transition);
        assert!(parent.open_child("stage").is_err_and(|error| {
            matches!(error, LoaderError::Io(error) if error.kind() == io::ErrorKind::NotFound)
        }));
    }

    #[test]
    fn tree_cleanup_never_deletes_a_replacement_binding() {
        let (_temporary, root) = managed_test_root("tree-replacement");
        let parent = root.create_child_new("parent").expect("create parent");
        let stage = parent.create_child_new("stage").expect("create original stage");
        let parent_descriptor = ManagedDirDescriptor::capture(&parent);
        let stage_descriptor = ManagedDirDescriptor::capture(&stage);
        parent
            .remove_empty_child(&stage)
            .expect("remove original stage");
        let replacement = parent
            .create_child_new("stage")
            .expect("create replacement stage");
        replacement
            .write_new_exact("keep.bin", b"replacement")
            .expect("write replacement payload");

        let root_owner = root.inner.root.clone();
        let transition = root_owner.transition();
        let cleanup = retain_tree_cleanup(
            &transition,
            parent_descriptor,
            PortableFileName::new_exact("stage").expect("stage name"),
            stage_descriptor,
        );
        assert!(
            cleanup.is_none(),
            "a replaced binding must terminate cleanup without touching replacement bytes"
        );
        drop(transition);
        assert!(
            replacement
                .inspect_regular_file("keep.bin")
                .expect("inspect replacement")
                .is_some()
        );
    }

    #[cfg(unix)]
    #[test]
    fn immediate_tree_cleanup_failure_is_retained_until_retry() {
        use std::fs as test_fs;
        use std::os::unix::fs::symlink;

        let (temporary, root) = managed_test_root("cleanup-retention");
        let parent = root.create_child_new("parent").expect("create parent");
        let stage = parent.create_child_new("stage").expect("create stage");
        let link = temporary.path().join("parent/stage/unsupported-link");
        symlink("missing-target", &link).expect("create unsupported stage entry");
        let outcome = cleanup_tree_failure(
            &parent,
            &PortableFileName::new_exact("stage").expect("stage name"),
            stage,
            ManagedTreeCopyFailure::UnsupportedEntry,
        );
        assert!(matches!(
            outcome,
            ManagedTreeCopyOutcome::CleanupRetained { .. }
        ));
        assert_eq!(
            root.inner
                .root
                .continuations
                .lock()
                .expect("continuation registry")
                .receipts
                .len(),
            1
        );

        test_fs::remove_file(link).expect("remove unsupported entry");
        root.inner.root.settle().expect("retry retained cleanup");
        assert!(parent.open_child("stage").is_err_and(|error| {
            matches!(error, LoaderError::Io(error) if error.kind() == io::ErrorKind::NotFound)
        }));
    }

    #[test]
    fn cached_root_reopen_retries_retained_tree_cleanup() {
        let (_temporary, root) = managed_test_root("continuation-reopen");
        let stage = root.create_child_new("stage").expect("create stage");
        let owner = root.inner.root.clone();
        {
            let transition = owner.transition();
            owner.retain_continuation_locked(
                &transition,
                ManagedEffectContinuation::TreeCleanup {
                    parent: ManagedDirDescriptor::capture(&root),
                    stage_name: PortableFileName::new_exact("stage").expect("stage name"),
                    stage: ManagedDirDescriptor::capture(&stage),
                },
            );
        }
        drop(stage);
        let reopened = ManagedDir::open_root(root.path())
            .expect("cached reopen settles retained tree cleanup");
        assert!(Arc::ptr_eq(&owner, &reopened.inner.root));
        assert!(reopened.open_child("stage").is_err_and(|error| {
            matches!(error, LoaderError::Io(error) if error.kind() == io::ErrorKind::NotFound)
        }));
        assert!(
            owner
                .continuations
                .lock()
                .expect("continuation registry")
                .receipts
                .is_empty()
        );
    }

    #[test]
    fn retained_tree_owner_retries_after_request_handles_drop() {
        let (_temporary, root) = managed_test_root("retained-tree-owner");
        let tree_owner = ManagedTreeDirectory {
            directory: root.clone(),
        };
        let stage = root.create_child_new("stage").expect("create stage");
        let owner = root.inner.root.clone();
        {
            let transition = owner.transition();
            owner.retain_continuation_locked(
                &transition,
                ManagedEffectContinuation::TreeCleanup {
                    parent: ManagedDirDescriptor::capture(&root),
                    stage_name: PortableFileName::new_exact("stage").expect("stage name"),
                    stage: ManagedDirDescriptor::capture(&stage),
                },
            );
        }
        drop(stage);
        drop(root);
        drop(owner);

        tree_owner
            .settle()
            .expect("retained tree owner settles cleanup");
        assert!(
            tree_owner
                .open_child("stage")
                .expect("inspect settled stage")
                .is_none()
        );
    }

    #[test]
    fn cached_root_reopen_refuses_a_replaced_lexical_binding() {
        use std::fs as test_fs;

        let temporary = tempfile::Builder::new()
            .prefix("axial-managed-fs-root-replacement-")
            .tempdir()
            .expect("temporary parent");
        let root_path = temporary.path().join("root");
        let moved_path = temporary.path().join("moved-root");
        test_fs::create_dir(&root_path).expect("create original root");
        let root = ManagedDir::open_root(&root_path).expect("open original root");

        test_fs::rename(&root_path, &moved_path).expect("move original root binding");
        test_fs::create_dir(&root_path).expect("create replacement root");
        test_fs::write(root_path.join("replacement.marker"), b"replacement")
            .expect("mark replacement root");

        assert!(
            ManagedDir::open_root(&root_path).is_err(),
            "cached reopen followed the moved root instead of refusing the replacement binding"
        );
        assert_eq!(
            test_fs::read(root_path.join("replacement.marker")).expect("read replacement marker"),
            b"replacement"
        );
        drop(root);
    }

    #[tokio::test]
    async fn promotion_replace_and_move_project_exact_wrapper_identity() {
        let (_temporary, root) = managed_test_root("terminal-projection");
        let guard = root
            .write_new_exact_retained("source.bin", b"first")
            .expect("promote source");
        let destination = root
            .create_child_new("destination")
            .expect("create destination");
        root.rename_guarded_file_no_replace("source.bin", &guard, &destination, "moved.bin")
            .expect("move guarded file");
        assert!(
            destination
                .file_guard_matches("moved.bin", &guard)
                .expect("validate moved identity")
        );
        destination
            .write_exact("moved.bin", b"second")
            .await
            .expect("replace moved file");
        let replaced = destination
            .inspect_regular_file("moved.bin")
            .expect("inspect replaced file")
            .expect("replaced file");
        assert_eq!(
            destination
                .read_guarded_file_bounded("moved.bin", &replaced, 6)
                .expect("read replaced file"),
            b"second"
        );
    }
}
