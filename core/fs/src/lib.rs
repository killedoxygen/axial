mod platform;
mod transient;

pub use transient::{
    TransientCreationObligation, TransientDestination, TransientDestinationBatch,
    TransientDestinationCancelObligation, TransientDestinationCancelOutcome,
    TransientDiscardObligation, TransientDiscardOutcome, TransientPublicationBatch,
    TransientPublicationBatchCreateFailure, TransientPublicationBatchObligation,
    TransientPublicationBatchOutcome, TransientPublicationMember, TransientStage,
    TransientStageCreateOutcome,
    TransientStageSealFailure, TransientStageSealed,
};

use std::borrow::Borrow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

const ROOT_LEASE_NAME: &str = ".axial-root.lease";
const MAX_LEAF_UNITS: usize = 255;
const MAX_STAGE_ATTEMPTS: usize = 32;
const MAX_DIRECTORY_LIST_ENTRIES: usize = 100_000;
const MAX_OUTSTANDING_EFFECTS: usize = 512;
const MAX_FILE_RANGE_BYTES: usize = 4 * 1024;

macro_rules! impl_redacted_debug {
    ($type:ty) => {
        impl fmt::Debug for $type {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter
                    .debug_struct(stringify!($type))
                    .finish_non_exhaustive()
            }
        }
    };
}

#[derive(Clone, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct LeafName(OsString);

impl LeafName {
    pub fn new(value: impl Into<OsString>) -> Result<Self, InvalidLeafName> {
        let value = value.into();
        validate_leaf_name(&value)?;
        Ok(Self(value))
    }

    pub fn as_os_str(&self) -> &OsStr {
        &self.0
    }
}

pub fn leaf_names_equivalent(first: &OsStr, second: &OsStr) -> bool {
    platform::leaf_names_equal(first, second)
}

#[derive(Clone, Eq, Hash, PartialEq)]
pub struct LeafNameEquivalenceKey(Vec<u8>);

impl Borrow<[u8]> for LeafNameEquivalenceKey {
    fn borrow(&self) -> &[u8] {
        &self.0
    }
}

impl fmt::Debug for LeafNameEquivalenceKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LeafNameEquivalenceKey")
            .finish_non_exhaustive()
    }
}

/// Returns the small set of lookup keys needed to find every name that the
/// platform or the portable spelling rules can consider equivalent.
pub fn leaf_name_equivalence_keys(name: &OsStr) -> Vec<LeafNameEquivalenceKey> {
    platform::leaf_name_equivalence_keys(name)
        .into_iter()
        .map(LeafNameEquivalenceKey)
        .collect()
}

impl fmt::Debug for LeafName {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_tuple("LeafName").finish()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("filesystem capability leaf name is invalid")]
pub struct InvalidLeafName;

fn validate_leaf_name(value: &OsStr) -> Result<(), InvalidLeafName> {
    if value.is_empty() || value == OsStr::new(".") || value == OsStr::new("..") {
        return Err(InvalidLeafName);
    }

    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;

        let bytes = value.as_bytes();
        if bytes.len() > MAX_LEAF_UNITS || bytes.iter().any(|byte| matches!(byte, 0 | b'/')) {
            return Err(InvalidLeafName);
        }
    }

    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;

        let units = value.encode_wide().collect::<Vec<_>>();
        if units.len() > MAX_LEAF_UNITS
            || units
                .iter()
                .any(|unit| matches!(*unit, 0 | 0x2f | 0x3a | 0x5c))
        {
            return Err(InvalidLeafName);
        }
    }

    #[cfg(not(any(unix, windows)))]
    if value.to_string_lossy().contains(['\0', '/', '\\']) {
        return Err(InvalidLeafName);
    }

    Ok(())
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DirectoryIdentity {
    session: [u8; 16],
    physical: platform::Identity,
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct DirectoryFilesystemIdentity(platform::Identity);

impl_redacted_debug!(DirectoryFilesystemIdentity);

#[derive(Clone, Copy, Eq, PartialEq)]
pub struct DirectoryRevision {
    identity: DirectoryIdentity,
    stamp: platform::DirectoryStamp,
}

impl_redacted_debug!(DirectoryRevision);

impl fmt::Debug for DirectoryIdentity {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryIdentity")
            .finish_non_exhaustive()
    }
}

impl DirectoryIdentity {
    pub fn same_filesystem_object(self, other: Self) -> bool {
        self.physical == other.physical
    }

    pub fn filesystem_identity(self) -> DirectoryFilesystemIdentity {
        DirectoryFilesystemIdentity(self.physical)
    }

}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EntryKind {
    File,
    Directory,
    Link,
    Other,
}

#[derive(Clone)]
pub struct DirectoryEntry {
    name: OsString,
    kind: EntryKind,
    parent: DirectoryIdentity,
}

impl fmt::Debug for DirectoryEntry {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("DirectoryEntry")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

impl DirectoryEntry {
    pub fn name(&self) -> &OsStr {
        &self.name
    }

    pub fn utf8_name(&self) -> Option<&str> {
        self.name.to_str()
    }

    pub fn kind(&self) -> EntryKind {
        self.kind
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DirectoryListingState {
    Complete,
    Truncated,
}

#[derive(Debug)]
pub struct DirectoryListing {
    entries: Vec<DirectoryEntry>,
    state: DirectoryListingState,
}

impl DirectoryListing {
    pub fn entries(&self) -> &[DirectoryEntry] {
        &self.entries
    }

    pub fn state(&self) -> DirectoryListingState {
        self.state
    }
}

#[must_use = "directory creation effects must be explicitly settled or preserved"]
#[derive(Debug)]
pub enum DirectoryCreateOutcome {
    Created(Directory),
    NoEffect(io::Error),
    CreatedUnclassified {
        error: io::Error,
        preservation: DirectoryCreatePreservation,
    },
    AppliedUnverified(DirectoryCreateObligation),
}

#[must_use = "an unclassified created directory must be explicitly acknowledged as preserved"]
pub struct DirectoryCreatePreservation {
    token: DirectoryCreateEffectToken,
}

impl_redacted_debug!(DirectoryCreatePreservation);

impl DirectoryCreatePreservation {
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return Err(self),
        };
        match authority.acknowledge_unclassified_directory_create(
            &operation,
            &mut self.token,
        ) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }

    fn acknowledge_preserved_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        let operation = match authority.enter_directory_create_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(_) => return Err(self),
        };
        match authority.acknowledge_unclassified_directory_create(
            &operation,
            &mut self.token,
        ) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }

    fn transfer_to_reset(mut self, permit: &DrainRecoveryPermit) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        match authority.transfer_unclassified_directory_create_to_reset(
            permit,
            &mut self.token,
        ) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }
}

#[must_use = "directory create obligations must be reconciled"]
pub struct DirectoryCreateObligation {
    error: io::Error,
    token: DirectoryCreateEffectToken,
}

impl_redacted_debug!(DirectoryCreateObligation);

impl DirectoryCreateObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> DirectoryCreateResolution {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return DirectoryCreateResolution::Indeterminate(self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return DirectoryCreateResolution::Indeterminate(self),
        };
        match finish_directory_create(&authority, &operation, &mut self.token) {
            Ok(directory) => DirectoryCreateResolution::Created(directory),
            Err(_) => DirectoryCreateResolution::Indeterminate(self),
        }
    }
}

fn finish_directory_create(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    reservation: &mut DirectoryCreateEffectToken,
) -> io::Result<Directory> {
    let mut guard = authority.take_directory_create(operation, reservation)?;
    if guard.record().phase != DirectoryCreateEffectPhase::Applied {
        return Err(io::Error::other(
            "directory create effect is not yet classified",
        ));
    }
    guard.record().parent.validate(operation)?;
    let created = guard
        .record()
        .created
        .as_ref()
        .ok_or_else(|| io::Error::other("directory create authority is not retained"))?;
    let identity = platform::directory_identity(created)?;
    if platform::directory_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        identity,
    )? != platform::BindingState::Exact
    {
        return Err(identity_changed("created directory binding is not exact"));
    }
    let (ordinary, ordinary_identity) = platform::open_directory(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
    )?;
    if ordinary_identity != identity {
        return Err(identity_changed(
            "created directory changed before least-authority admission",
        ));
    }
    let parent = guard.record().parent.clone();
    let name = guard.record().name.clone();
    let directory = Directory::from_handle(
        ordinary,
        authority.identity(identity),
        Arc::downgrade(authority),
        Some(DirectoryParent {
            directory: parent,
            name: name.as_os_str().to_os_string(),
        }),
    );
    directory.validate(operation)?;
    drop(guard.record_mut().created.take());
    guard.disarm(reservation, operation);
    Ok(directory)
}

#[must_use = "directory create resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryCreateResolution {
    Created(Directory),
    Indeterminate(DirectoryCreateObligation),
}

#[must_use = "file create effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileCreateOutcome {
    Created(StagedFile),
    NoEffect(io::Error),
    AppliedUnverified(FileCreateObligation),
}

#[must_use = "file create obligations must be reconciled"]
pub struct FileCreateObligation {
    error: io::Error,
    token: StageCreateToken,
}

impl_redacted_debug!(FileCreateObligation);

impl FileCreateObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileCreateResolution {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return FileCreateResolution::Indeterminate(self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return FileCreateResolution::Indeterminate(self),
        };
        match finish_stage_create(&authority, &operation, &mut self.token) {
            Ok(staged) => FileCreateResolution::Created(staged),
            Err(_) => FileCreateResolution::Indeterminate(self),
        }
    }
}

fn finish_stage_create(
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    reservation: &mut StageCreateToken,
) -> io::Result<StagedFile> {
    let mut guard = authority.take_stage_create(operation, reservation)?;
    if guard.record().phase != StageCreatePhase::Applied {
        return Err(io::Error::other("stage create effect is not yet classified"));
    }
    guard.record().parent.validate(operation)?;
    let created = guard
        .record()
        .created
        .as_ref()
        .ok_or_else(|| io::Error::other("stage create authority is not retained"))?;
    let identity = platform::file_identity(created)?;
    if platform::file_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        identity,
    )? != platform::BindingState::Exact
    {
        return Err(identity_changed("created stage binding is not exact"));
    }
    let cleanup = platform::clone_stage_cleanup(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        created,
        identity,
    )?;
    let parent = guard.record().parent.clone();
    let name = guard.record().name.clone();
    let stage_token = authority.register_stage_record(
        parent.clone(),
        name.clone(),
        identity,
        cleanup,
        operation,
    )?;
    let handle = guard
        .record_mut()
        .created
        .take()
        .expect("classified stage create retains its handle");
    guard.transfer(reservation, operation);
    Ok(StagedFile {
        file: FileCapability::new(handle, identity, parent, name, Arc::downgrade(authority)),
        token: stage_token,
    })
}

#[must_use = "file create resolutions must be handled"]
#[derive(Debug)]
pub enum FileCreateResolution {
    Created(StagedFile),
    Indeterminate(FileCreateObligation),
}

#[must_use = "file promotion effects must be explicitly settled"]
#[derive(Debug)]
pub enum FilePromotionOutcome {
    Applied(FileCapability),
    NoEffect {
        error: io::Error,
        staged: SealedStagedFile,
    },
    AppliedUnverified(FilePromotionObligation),
}

#[must_use = "file promotion obligations must be reconciled"]
pub struct FilePromotionObligation {
    error: io::Error,
    retained: SealedStagedFile,
    destination: Directory,
    destination_name: LeafName,
}

impl fmt::Debug for FilePromotionObligation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FilePromotionObligation")
            .finish_non_exhaustive()
    }
}

impl FilePromotionObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FilePromotionResolution {
        let authority = match self.retained.file.parent.authority() {
            Ok(authority) => authority,
            Err(_) => return FilePromotionResolution::Indeterminate(self),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return FilePromotionResolution::Indeterminate(self),
        };
        if self.retained.file.parent.validate(&operation).is_err()
            || self.destination.validate(&operation).is_err()
            || platform::file_identity(&self.retained.file.handle).ok()
                != Some(self.retained.file.identity)
        {
            return FilePromotionResolution::Indeterminate(self);
        }
        let source = platform::file_binding_state(
            &self.retained.file.parent.inner.handle,
            self.retained.file.name.as_os_str(),
            self.retained.file.identity,
        );
        let destination = platform::file_binding_state(
            &self.destination.inner.handle,
            self.destination_name.as_os_str(),
            self.retained.file.identity,
        );
        match (source, destination) {
            (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
                if sync_rename_parents(&self.retained.file.parent, &self.destination).is_err()
                    || self.destination.validate(&operation).is_err()
                {
                    return FilePromotionResolution::Indeterminate(self);
                }
                let handle = match platform::open_file(
                    &self.destination.inner.handle,
                    self.destination_name.as_os_str(),
                ) {
                    Ok(handle)
                        if platform::file_identity(&handle).ok()
                            == Some(self.retained.file.identity) => handle,
                    _ => return FilePromotionResolution::Indeterminate(self),
                };
                let applied = FileCapability::new(
                    handle,
                    self.retained.file.identity,
                    self.destination.clone(),
                    self.destination_name.clone(),
                    self.retained.file.authority.clone(),
                );
                if applied.validate(&operation).is_err()
                    || self.retained.token.disarm().is_err()
                {
                    return FilePromotionResolution::Indeterminate(self);
                }
                FilePromotionResolution::Applied(applied)
            }
            (Ok(platform::BindingState::Exact), _) => {
                if self.retained.token.update(StageRegistryPhase::Sealed).is_err()
                    || self.retained.file.validate(&operation).is_err()
                {
                    return FilePromotionResolution::Indeterminate(self);
                }
                FilePromotionResolution::NoEffect(self.retained)
            }
            _ => FilePromotionResolution::Indeterminate(self),
        }
    }
}

#[must_use = "file promotion resolutions must be handled"]
#[derive(Debug)]
pub enum FilePromotionResolution {
    Applied(FileCapability),
    NoEffect(SealedStagedFile),
    Indeterminate(FilePromotionObligation),
}

#[must_use = "file move effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileMoveOutcome {
    Applied(FileCapability),
    NoEffect {
        error: io::Error,
        file: FileCapability,
    },
    AppliedUnverified(FileMoveObligation),
}

#[must_use = "file move resolutions must be handled"]
#[derive(Debug)]
pub enum FileMoveResolution {
    Applied(FileCapability),
    NoEffect(FileCapability),
    Indeterminate(FileMoveObligation),
}

#[must_use = "file move obligations must be reconciled"]
pub struct FileMoveObligation {
    error: io::Error,
    file: Option<FileCapability>,
    destination: Directory,
    destination_name: LeafName,
    reported_success: bool,
    token: MoveEffectToken,
}

impl_redacted_debug!(FileMoveObligation);

impl FileMoveObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileMoveResolution {
        let file = self.file.take().expect("file move obligation retains its file");
        match settle_file_move(
            file,
            &self.destination,
            &self.destination_name,
            self.reported_success,
            &mut self.token,
        ) {
            Ok((true, file)) => FileMoveResolution::Applied(file),
            Ok((false, file)) => FileMoveResolution::NoEffect(file),
            Err(file) => {
                self.file = Some(file);
                FileMoveResolution::Indeterminate(self)
            }
        }
    }
}

#[must_use = "directory move effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryMoveOutcome {
    Applied(Directory),
    NoEffect {
        error: io::Error,
        directory: Directory,
    },
    AppliedUnverified(DirectoryMoveObligation),
}

#[must_use = "directory move resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryMoveResolution {
    Applied(Directory),
    NoEffect(Directory),
    Indeterminate(DirectoryMoveObligation),
}

#[must_use = "directory move obligations must be reconciled"]
pub struct DirectoryMoveObligation {
    error: io::Error,
    directory: Option<Directory>,
    destination: Directory,
    destination_name: LeafName,
    reported_success: bool,
    token: MoveEffectToken,
}

impl_redacted_debug!(DirectoryMoveObligation);

impl DirectoryMoveObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> DirectoryMoveResolution {
        let directory = self
            .directory
            .take()
            .expect("directory move obligation retains its directory");
        match settle_directory_move(
            directory,
            &self.destination,
            &self.destination_name,
            self.reported_success,
            &mut self.token,
        ) {
            Ok((true, directory)) => DirectoryMoveResolution::Applied(directory),
            Ok((false, directory)) => DirectoryMoveResolution::NoEffect(directory),
            Err(directory) => {
                self.directory = Some(directory);
                DirectoryMoveResolution::Indeterminate(self)
            }
        }
    }
}

pub enum ReplaceDestination {
    Vacant { parent: Directory, name: LeafName },
    Existing(FileParkRequest),
}

impl_redacted_debug!(ReplaceDestination);

#[must_use = "file replacement effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileReplaceOutcome {
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        error: io::Error,
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
    AppliedUnverified(FileReplaceObligation),
}

#[must_use = "file replacement resolutions must be handled"]
#[derive(Debug)]
pub enum FileReplaceResolution {
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
    Indeterminate(FileReplaceObligation),
}

struct ExpectedContentReceipt {
    authority: Weak<CapabilityAuthority>,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
    sha256: [u8; 32],
}

impl ExpectedContentReceipt {
    fn capture(request: &FileParkRequest) -> Self {
        Self {
            authority: request.expected.revision.authority.clone(),
            identity: request.expected.revision.identity,
            size: request.expected.revision.size,
            stamp: request.expected.revision.stamp,
            sha256: request.expected.sha256,
        }
    }

    fn rebuild(self, file: FileCapability) -> FileParkRequest {
        FileParkRequest {
            file,
            expected: ExpectedFileContent {
                revision: FileRevision {
                    authority: self.authority,
                    identity: self.identity,
                    size: self.size,
                    stamp: self.stamp,
                },
                sha256: self.sha256,
            },
        }
    }
}

enum FileReplaceObligationState {
    Parking {
        park: FileParkObligation,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
    Promoting {
        promotion: FilePromotionObligation,
        displaced: Option<ParkedFile>,
        fallback: ReplaceDestination,
        receipt: Option<ExpectedContentReceipt>,
    },
    RestoreParked {
        parked: ParkedFile,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
    RestoreObligation {
        restore: FileRestoreObligation,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
}

#[must_use = "file replacement obligations must be reconciled"]
pub struct FileReplaceObligation {
    error: io::Error,
    state: Option<FileReplaceObligationState>,
}

impl_redacted_debug!(FileReplaceObligation);

impl FileReplaceObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileReplaceResolution {
        let state = self.state.take().expect("replace obligation retains state");
        settle_file_replace(state).unwrap_or_else(|state| {
            self.state = Some(state);
            FileReplaceResolution::Indeterminate(self)
        })
    }
}

#[must_use = "file park effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileParkOutcome {
    Parked(ParkedFile),
    NoEffect { error: io::Error, request: FileParkRequest },
    AppliedUnverified(FileParkObligation),
}

#[must_use = "file park resolutions must be handled"]
#[derive(Debug)]
pub enum FileParkResolution {
    Parked(ParkedFile),
    NoEffect(FileParkRequest),
    Indeterminate(FileParkObligation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileParkPhase {
    Parking,
    RestoringRejectedReceipt,
}

#[must_use = "file park obligations must be reconciled"]
pub struct FileParkObligation {
    error: io::Error,
    request: Option<FileParkRequest>,
    token: FileParkRegistryToken,
    park_name: LeafName,
    phase: FileParkPhase,
    digest_verified: bool,
}

impl_redacted_debug!(FileParkObligation);

impl FileParkObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(self) -> FileParkResolution {
        settle_file_park(self, false)
    }

    pub fn restore(self) -> FileParkResolution {
        settle_file_park(self, true)
    }
}

#[must_use = "a parked file must be removed, restored, acknowledged as preserved, or retained as an obligation"]
pub struct ParkedFile {
    parent: Directory,
    original_name: LeafName,
    park_name: LeafName,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
    verified: bool,
    token: FileParkRegistryToken,
    authority: Weak<CapabilityAuthority>,
}

impl_redacted_debug!(ParkedFile);

#[must_use = "failed preservation acknowledgement retains the parked file authority"]
pub struct FileParkPreservationError {
    error: io::Error,
    parked: ParkedFile,
}

impl_redacted_debug!(FileParkPreservationError);

impl FileParkPreservationError {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_parked(self) -> ParkedFile {
        self.parked
    }
}

#[must_use = "file removal effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileRemovalOutcome {
    Removed,
    NoEffect { error: io::Error, parked: ParkedFile },
    AppliedUnverified(FileRemovalObligation),
}

#[must_use = "file removal resolutions must be handled"]
#[derive(Debug)]
pub enum FileRemovalResolution {
    Removed,
    NoEffect(ParkedFile),
    Indeterminate(FileRemovalObligation),
}

#[must_use = "file removal obligations must be reconciled"]
pub struct FileRemovalObligation {
    error: io::Error,
    parked: Option<ParkedFile>,
}

impl_redacted_debug!(FileRemovalObligation);

impl FileRemovalObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileRemovalResolution {
        let parked = self.parked.take().expect("removal obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRemovalResolution {
        let parked = self.parked.take().expect("removal obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_admitted(
        mut self,
        mut parked: ParkedFile,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            drop(operation);
            return self.retain(parked);
        }
        let guard = match authority.take_file_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_removed_file(
            &guard.record().parent.inner.handle,
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        ) {
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                FileRemovalResolution::Removed
            }
            Ok(()) => {
                drop(operation);
                self.parked = Some(parked);
                FileRemovalResolution::Indeterminate(self)
            }
            Err(_) => {
                drop(operation);
                self.parked = Some(parked);
                FileRemovalResolution::Indeterminate(self)
            }
        }
    }

    fn retain(mut self, parked: ParkedFile) -> FileRemovalResolution {
        self.parked = Some(parked);
        FileRemovalResolution::Indeterminate(self)
    }
}

#[must_use = "file restore effects must be explicitly settled"]
#[derive(Debug)]
pub enum FileRestoreOutcome {
    Restored(FileCapability),
    NoEffect { error: io::Error, parked: ParkedFile },
    AppliedUnverified(FileRestoreObligation),
}

#[must_use = "file restore resolutions must be handled"]
#[derive(Debug)]
pub enum FileRestoreResolution {
    Restored(FileCapability),
    NoEffect(ParkedFile),
    Indeterminate(FileRestoreObligation),
}

#[must_use = "file restore obligations must be reconciled"]
pub struct FileRestoreObligation {
    error: io::Error,
    parked: Option<ParkedFile>,
}

impl_redacted_debug!(FileRestoreObligation);

impl FileRestoreObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileRestoreResolution {
        let parked = self.parked.take().expect("restore obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRestoreResolution {
        let parked = self.parked.take().expect("restore obligation retains parked file");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_file_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_admitted(
        mut self,
        parked: ParkedFile,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRestoreResolution {
        match settle_file_restore_admitted(parked, authority, operation) {
            FileRestoreSettlement::Restored(file) => FileRestoreResolution::Restored(file),
            FileRestoreSettlement::NoEffect(parked) => FileRestoreResolution::NoEffect(parked),
            FileRestoreSettlement::Indeterminate(parked) => self.retain(parked),
        }
    }

    fn retain(mut self, parked: ParkedFile) -> FileRestoreResolution {
        self.parked = Some(parked);
        FileRestoreResolution::Indeterminate(self)
    }
}

enum FileRestoreSettlement {
    Restored(FileCapability),
    NoEffect(ParkedFile),
    Indeterminate(ParkedFile),
}

impl ParkedFile {
    pub fn validate_current(&self) -> io::Result<()> {
        let (operation, guard) = self.checkout_current()?;
        drop(guard);
        drop(operation);
        Ok(())
    }

    pub fn acknowledge_preserved(mut self) -> Result<(), FileParkPreservationError> {
        let (operation, guard) = match self.checkout_current() {
            Ok(current) => current,
            Err(error) => {
                return Err(FileParkPreservationError {
                    error,
                    parked: self,
                });
            }
        };
        guard.disarm(&mut self.token, &operation);
        Ok(())
    }

    fn checkout_current(&self) -> io::Result<(CapabilityOperation, FileParkRecordGuard)> {
        if !self.verified {
            return Err(identity_changed("unverified parked file has no exact receipt"));
        }
        let authority = self.authority()?;
        let operation = authority.enter_file_park(&self.token)?;
        let guard = authority.take_file_park(&operation, &self.token)?;
        if let Err(error) = self.validate_checked_out(&operation, guard.record()) {
            drop(guard);
            return Err(error);
        }
        Ok((operation, guard))
    }

    pub fn remove(mut self) -> FileRemovalOutcome {
        if !self.verified {
            return FileRemovalOutcome::NoEffect {
                error: identity_changed("unverified parked file can only be restored"),
                parked: self,
            };
        }
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        if let Err(error) = self.validate(&operation) {
            return FileRemovalOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_file(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                FileRemovalOutcome::Removed
            }
            Ok(()) => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                error: identity_changed("file removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    FileRemovalOutcome::NoEffect { error, parked: self }
                }
                _ => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn remove_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        self.remove_admitted(authority, operation)
    }

    fn remove_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRemovalOutcome {
        if !self.verified {
            return FileRemovalOutcome::NoEffect {
                error: identity_changed("unverified parked file can only be restored"),
                parked: self,
            };
        }
        if let Err(error) = self.validate(&operation) {
            return FileRemovalOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return FileRemovalOutcome::NoEffect { error, parked: self },
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_file(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                FileRemovalOutcome::Removed
            }
            Ok(()) => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                error: identity_changed("file removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    FileRemovalOutcome::NoEffect { error, parked: self }
                }
                _ => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    pub fn restore(mut self) -> FileRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_file(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = FileCapability::new(
                    handle,
                    self.identity,
                    self.parent.clone(),
                    self.original_name.clone(),
                    self.authority.clone(),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    FileRestoreOutcome::Restored(restored)
                } else {
                    FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error: identity_changed("restored file lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    FileRestoreOutcome::NoEffect { error, parked: self }
                }
                _ => FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn restore_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> FileRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        self.restore_admitted(authority, operation)
    }

    fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return FileRestoreOutcome::NoEffect { error, parked: self },
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_file(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = FileCapability::new(
                    handle,
                    self.identity,
                    self.parent.clone(),
                    self.original_name.clone(),
                    self.authority.clone(),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    FileRestoreOutcome::Restored(restored)
                } else {
                    FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error: identity_changed("restored file lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    FileRestoreOutcome::NoEffect { error, parked: self }
                }
                _ => FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.authority.upgrade().ok_or_else(stale_capability)
    }

    fn binding_state(&self) -> io::Result<platform::BindingState> {
        platform::file_binding_state(
            &self.parent.inner.handle,
            self.park_name.as_os_str(),
            self.identity,
        )
    }

    fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        {
            return Err(identity_changed("parked file capability changed"));
        }
        let registered = {
            let state = operation.authority.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            state.file_parks.get(&self.token.id).is_some_and(|record| {
                record.phase == FileParkRegistryPhase::Live
                    && record.identity == self.identity
                    && record.size == self.size
                    && record.stamp == self.stamp
                    && record.name == self.park_name
                    && record.original_name == self.original_name
            })
        };
        if !registered || self.binding_state()? != platform::BindingState::Exact {
            return Err(identity_changed("parked file capability changed"));
        }
        Ok(())
    }

    fn validate_checked_out(
        &self,
        operation: &CapabilityOperation,
        record: &FileParkRegistryRecord,
    ) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || record.phase != FileParkRegistryPhase::Live
            || record.identity != self.identity
            || record.size != self.size
            || record.stamp != self.stamp
            || record.name != self.park_name
            || record.original_name != self.original_name
            || platform::parked_file_receipt_fields(&record.cleanup)? != (self.size, self.stamp)
            || platform::file_binding_state(
                &self.parent.inner.handle,
                self.park_name.as_os_str(),
                self.identity,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed("parked file capability changed"));
        }
        self.parent.validate(operation)
    }
}

fn settle_file_restore_admitted(
    mut parked: ParkedFile,
    authority: Arc<CapabilityAuthority>,
    operation: CapabilityOperation,
) -> FileRestoreSettlement {
    if parked.parent.validate(&operation).is_err() {
        return FileRestoreSettlement::Indeterminate(parked);
    }
    let guard = match authority.take_file_park(&operation, &parked.token) {
        Ok(guard) => guard,
        Err(_) => return FileRestoreSettlement::Indeterminate(parked),
    };
    match platform::settle_restored_file(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        &guard.record().cleanup,
        guard.record().identity,
        guard.record().original_name.as_os_str(),
    ) {
        Ok(handle) => {
            let restored = FileCapability::new(
                handle,
                parked.identity,
                parked.parent.clone(),
                parked.original_name.clone(),
                parked.authority.clone(),
            );
            if restored.validate(&operation).is_ok() {
                guard.disarm(&mut parked.token, &operation);
                FileRestoreSettlement::Restored(restored)
            } else {
                drop(guard);
                FileRestoreSettlement::Indeterminate(parked)
            }
        }
        Err(_) => {
            drop(guard);
            match parked.binding_state() {
            Ok(platform::BindingState::Exact) if parked.validate(&operation).is_ok() => {
                FileRestoreSettlement::NoEffect(parked)
            }
            _ => FileRestoreSettlement::Indeterminate(parked),
            }
        }
    }
}

#[must_use = "directory park effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryParkOutcome {
    Parked(ParkedDirectory),
    NoEffect { error: io::Error, directory: Directory },
    AppliedUnverified(DirectoryParkObligation),
}

#[must_use = "directory park resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryParkResolution {
    Parked(ParkedDirectory),
    NoEffect(Directory),
    Indeterminate(DirectoryParkObligation),
}

#[must_use = "directory park obligations must be reconciled"]
pub struct DirectoryParkObligation {
    error: io::Error,
    parent: Directory,
    directory: Option<Directory>,
    original_name: LeafName,
    token: DirectoryParkRegistryToken,
    park_name: LeafName,
}

impl_redacted_debug!(DirectoryParkObligation);

impl DirectoryParkObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(self) -> DirectoryParkResolution {
        settle_directory_park(self, false)
    }

    pub fn restore(self) -> DirectoryParkResolution {
        settle_directory_park(self, true)
    }
}

#[must_use = "a parked directory must be removed, restored, or retained as an obligation"]
pub struct ParkedDirectory {
    parent: Directory,
    original_name: LeafName,
    park_name: LeafName,
    identity: DirectoryIdentity,
    token: DirectoryParkRegistryToken,
    authority: Weak<CapabilityAuthority>,
}

impl_redacted_debug!(ParkedDirectory);

#[must_use = "a retained directory tree removal must be retried or transferred to an effect owner"]
pub struct RetainedDirectoryTreeRemoval {
    parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(RetainedDirectoryTreeRemoval);

impl RetainedDirectoryTreeRemoval {
    fn new(parked: ParkedDirectory) -> Self {
        Self {
            parked: Some(parked),
        }
    }

    pub fn retry(mut self) -> DirectoryTreeRemovalOutcome {
        self.parked
            .take()
            .expect("retained tree removal owns its parked directory")
            .remove_tree()
    }
}

impl Drop for RetainedDirectoryTreeRemoval {
    fn drop(&mut self) {
        if self.parked.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "directory removal effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryRemovalOutcome {
    Removed,
    NoEffect { error: io::Error, parked: ParkedDirectory },
    AppliedUnverified(DirectoryRemovalObligation),
}

#[must_use = "directory removal resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryRemovalResolution {
    Removed,
    NoEffect(ParkedDirectory),
    Indeterminate(DirectoryRemovalObligation),
}

#[must_use = "directory removal obligations must be reconciled"]
pub struct DirectoryRemovalObligation {
    error: io::Error,
    parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryRemovalObligation);

#[must_use = "directory tree removal effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryTreeRemovalOutcome {
    Removed,
    /// Native traversal did not start; the exact parked root remains owned by
    /// a retry-only carrier.
    Retained {
        error: io::Error,
        retained: RetainedDirectoryTreeRemoval,
    },
    /// The parked root topology could not be proven after the attempt.
    Indeterminate(DirectoryTreeRemovalObligation),
}

#[must_use = "directory tree removal resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryTreeRemovalResolution {
    Removed,
    Indeterminate(DirectoryTreeRemovalObligation),
}

#[must_use = "directory tree removal obligations must be reconciled"]
pub struct DirectoryTreeRemovalObligation {
    error: io::Error,
    parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryTreeRemovalObligation);

impl Drop for DirectoryTreeRemovalObligation {
    fn drop(&mut self) {
        if self.parked.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "directory restore effects must be explicitly settled"]
#[derive(Debug)]
pub enum DirectoryRestoreOutcome {
    Restored(Directory),
    NoEffect { error: io::Error, parked: ParkedDirectory },
    AppliedUnverified(DirectoryRestoreObligation),
}

#[must_use = "directory restore resolutions must be handled"]
#[derive(Debug)]
pub enum DirectoryRestoreResolution {
    Restored(Directory),
    NoEffect(ParkedDirectory),
    Indeterminate(DirectoryRestoreObligation),
}

#[must_use = "directory restore obligations must be reconciled"]
pub struct DirectoryRestoreObligation {
    error: io::Error,
    parked: Option<ParkedDirectory>,
}

impl_redacted_debug!(DirectoryRestoreObligation);

#[derive(Debug, thiserror::Error)]
pub enum RootSessionError {
    #[error("running process image could not be retained")]
    ProcessImage(#[source] io::Error),
    #[error("application root could not be created")]
    Create(#[source] io::Error),
    #[error("application root is not an exact physical directory")]
    Open(#[source] io::Error),
    #[error("application root is already leased by another process")]
    Busy,
    #[error("application root lease could not be acquired")]
    Lease(#[source] io::Error),
}

#[must_use = "root acquisition effects must be explicitly acquired, cleaned up, or reconciled"]
#[derive(Debug)]
pub enum RootSessionAcquireOutcome {
    Acquired(RootSession),
    NoEffect(RootSessionError),
    AppliedUnverified(RootSessionAcquireObligation),
}

#[must_use = "absolute directory admission outcome must be handled"]
#[derive(Debug)]
pub enum AbsoluteDirectoryOutsideRootAdmission {
    Admitted(AdmittedAbsoluteDirectory),
    InsideRoot,
    Unavailable(io::Error),
}

#[must_use = "admitted root acquisition effects must be explicitly settled"]
#[derive(Debug)]
pub enum AdmittedRootSessionAcquireOutcome {
    Acquired(AdmittedRootSession),
    NoEffect(RootSessionError),
    AppliedUnverified(AdmittedRootSessionAcquireObligation),
}

#[must_use = "admitted root acquisition obligation must be reconciled or cleaned up"]
pub struct AdmittedRootSessionAcquireObligation {
    admission: Arc<AdmittedAbsoluteDirectoryInner>,
    obligation: Option<RootSessionAcquireObligation>,
}

pub struct AdmittedRootSession {
    admission: Arc<AdmittedAbsoluteDirectoryInner>,
    session: RootSession,
}

impl_redacted_debug!(AdmittedRootSessionAcquireObligation);
impl_redacted_debug!(AdmittedRootSession);

impl AdmittedRootSessionAcquireObligation {
    pub fn error(&self) -> &RootSessionError {
        self.obligation
            .as_ref()
            .expect("admitted root acquisition retains its obligation")
            .error()
    }

    pub fn reconcile(mut self) -> AdmittedRootSessionAcquireOutcome {
        let obligation = self
            .obligation
            .take()
            .expect("admitted root acquisition retains its obligation");
        match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => {
                AdmittedRootSessionAcquireOutcome::Acquired(AdmittedRootSession {
                    admission: Arc::clone(&self.admission),
                    session,
                })
            }
            RootSessionAcquireOutcome::NoEffect(error) => {
                AdmittedRootSessionAcquireOutcome::NoEffect(error)
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                self.obligation = Some(obligation);
                AdmittedRootSessionAcquireOutcome::AppliedUnverified(self)
            }
        }
    }

    pub fn cleanup(mut self) -> Result<(), Self> {
        let obligation = self
            .obligation
            .take()
            .expect("admitted root acquisition retains its obligation");
        match obligation.cleanup() {
            Ok(()) => Ok(()),
            Err(obligation) => {
                self.obligation = Some(obligation);
                Err(self)
            }
        }
    }
}

impl Drop for AdmittedRootSessionAcquireObligation {
    fn drop(&mut self) {
        if self.obligation.is_some() {
            std::process::abort();
        }
    }
}

impl AdmittedRootSession {
    pub fn identity(&self) -> DirectoryIdentity {
        self.session.identity()
    }

    pub fn root(&self) -> io::Result<Directory> {
        self.validate_retained_authority()?;
        let root = self.session.root()?;
        self.validate_retained_authority()?;
        Ok(root)
    }

    pub fn validate_retained_authority(&self) -> io::Result<()> {
        self.session.validate_retained_authority()?;
        let admitted_identity = platform::directory_identity(&self.admission.directory.inner.handle)?;
        if admitted_identity != self.admission.directory.inner.identity.physical
            || self.admission.directory.inner.identity.filesystem_identity()
            != self.session.identity().filesystem_identity()
        {
            return Err(identity_changed(
                "admitted root session changed physical identity",
            ));
        }
        Ok(())
    }
}

#[must_use = "partial root construction must be reconciled or cleaned up"]
pub struct RootSessionAcquireObligation {
    error: RootSessionError,
    construction: Option<platform::RootConstruction>,
    lease: Option<platform::LeaseAcquisitionObligation>,
    process_image: Option<platform::ProcessImageAncestry>,
}

impl_redacted_debug!(RootSessionAcquireObligation);

impl RootSessionAcquireObligation {
    pub fn error(&self) -> &RootSessionError {
        &self.error
    }

    pub fn reconcile(mut self) -> RootSessionAcquireOutcome {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            let identity = match platform::root_construction_identity(&construction) {
                Ok(identity) => identity,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            match platform::reconcile_lease_acquisition(root, lease) {
                Ok(lease) => {
                    let process_image = self
                        .process_image
                        .take()
                        .expect("root acquisition obligation retains process image ancestry");
                    return finish_root_session(
                        construction,
                        identity,
                        lease,
                        process_image,
                    );
                }
                Err(lease) => {
                    self.error = RootSessionError::Lease(copy_io_error(
                        platform::lease_acquisition_error(&lease),
                    ));
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            }
        }
        match platform::reconcile_root_construction(construction) {
            Ok(construction) => {
                let process_image = self
                    .process_image
                    .take()
                    .expect("root acquisition obligation retains process image ancestry");
                try_acquire_lease_and_finish_root(construction, process_image)
            }
            Err(error) => {
                let (error, construction) = error.into_parts();
                self.error = RootSessionError::Create(error);
                self.construction = construction;
                RootSessionAcquireOutcome::AppliedUnverified(self)
            }
        }
    }

    pub fn cleanup(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return Err(self);
                }
            };
            let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
            if let Err(lease) =
                platform::cleanup_lease_acquisition(root, lease_name.as_os_str(), lease)
            {
                self.error = RootSessionError::Lease(copy_io_error(
                    platform::lease_acquisition_error(&lease),
                ));
                self.construction = Some(construction);
                self.lease = Some(lease);
                return Err(self);
            }
        }
        match platform::cleanup_root_construction(construction) {
            Ok(()) => Ok(()),
            Err(error) => {
                let (error, construction) = error.into_parts();
                self.error = RootSessionError::Create(error);
                self.construction = construction;
                Err(self)
            }
        }
    }

    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if !platform::root_construction_has_unclassified(&construction) {
            self.construction = Some(construction);
            return Err(self);
        }
        if let Some(lease) = self.lease.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.lease = Some(lease);
                    return Err(self);
                }
            };
            let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
            if let Err(lease) =
                platform::cleanup_lease_acquisition(root, lease_name.as_os_str(), lease)
            {
                self.error = RootSessionError::Lease(copy_io_error(
                    platform::lease_acquisition_error(&lease),
                ));
                self.construction = Some(construction);
                self.lease = Some(lease);
                return Err(self);
            }
        }
        platform::acknowledge_preserved_root_construction(construction);
        Ok(())
    }
}

impl Drop for RootSessionAcquireObligation {
    fn drop(&mut self) {
        if self.construction.is_some() || self.lease.is_some() {
            std::process::abort();
        }
    }
}

fn copy_io_error(error: &io::Error) -> io::Error {
    io::Error::new(error.kind(), error.to_string())
}

const AUTHORITY_LIVE: u8 = 0;
const AUTHORITY_QUIESCING: u8 = 1;
const AUTHORITY_DRAINING: u8 = 2;
const AUTHORITY_RESETTING: u8 = 3;
const AUTHORITY_REVOKED: u8 = 4;
const MAX_EFFECT_OWNERS: usize = 256;
const MAX_EFFECTS_PER_OWNER: usize = 256;

thread_local! {
    static TERMINAL_EFFECT_SETTLEMENT_AUTHORITY: Cell<Option<(usize, u64)>> =
        const { Cell::new(None) };
}

struct TerminalEffectSettlementScope {
    previous: Option<(usize, u64)>,
}

impl TerminalEffectSettlementScope {
    fn begin(authority: &Arc<CapabilityAuthority>, owner_id: u64) -> io::Result<Self> {
        let current = (Arc::as_ptr(authority) as usize, owner_id);
        TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
            let previous = slot.get();
            if previous.is_some() && previous != Some(current) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "a different filesystem effect owner is already settling on this thread",
                ));
            }
            slot.set(Some(current));
            Ok(Self { previous })
        })
    }
}

impl Drop for TerminalEffectSettlementScope {
    fn drop(&mut self) {
        TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| slot.set(self.previous));
    }
}

fn terminal_effect_settlement_admits(authority: &CapabilityAuthority) -> bool {
    let authority = authority as *const CapabilityAuthority as usize;
    TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
        slot.get()
            .is_some_and(|(settling_authority, _)| settling_authority == authority)
    })
}

fn terminal_effect_settlement_admits_owner(
    authority: &CapabilityAuthority,
    owner_id: u64,
) -> bool {
    TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
        slot.get()
            == Some((authority as *const CapabilityAuthority as usize, owner_id))
    })
}

struct TerminalQuiescingRollback<'a> {
    authority: &'a CapabilityAuthority,
    armed: bool,
}

impl<'a> TerminalQuiescingRollback<'a> {
    fn new(authority: &'a CapabilityAuthority) -> Self {
        Self {
            authority,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for TerminalQuiescingRollback<'_> {
    fn drop(&mut self) {
        if self.armed {
            self.authority.restore_live_after_quiescing();
        }
    }
}

fn validate_terminal_registry_state(state: &OperationState) -> io::Result<()> {
    let registered_effects = state
        .stages
        .len()
        .checked_add(state.stage_creations.len())
        .and_then(|count| count.checked_add(state.directory_creations.len()))
        .and_then(|count| count.checked_add(state.file_parks.len()))
        .and_then(|count| count.checked_add(state.file_parks_checked_out))
        .and_then(|count| count.checked_add(state.directory_parks.len()))
        .and_then(|count| count.checked_add(state.directory_parks_checked_out))
        .and_then(|count| count.checked_add(state.moves.len()))
        .and_then(|count| count.checked_add(state.transients.len()))
        .ok_or_else(|| io::Error::other("filesystem effect registry count overflowed"))?;
    if state.outstanding_effects != registered_effects {
        return Err(io::Error::other(
            "filesystem effect registry accounting is inconsistent",
        ));
    }
    let registered_parks = state
        .file_parks
        .len()
        .checked_add(state.directory_parks.len())
        .ok_or_else(|| io::Error::other("filesystem park registry count overflowed"))?;
    if state.park_owners.len() != registered_parks
        || state.file_parks.iter().any(|(id, record)| {
            state.park_owners.get(&record.key()) != Some(&ParkRegistryOwner::File(*id))
        })
        || state.directory_parks.iter().any(|(id, record)| {
            state.park_owners.get(&record.key()) != Some(&ParkRegistryOwner::Directory(*id))
        })
    {
        return Err(io::Error::other(
            "filesystem park ownership accounting is inconsistent",
        ));
    }
    if !state.moves.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an unsettled move obligation",
        ));
    }
    if state
        .file_parks
        .values()
        .any(|record| record.phase != FileParkRegistryPhase::Abandoned)
        || state
            .directory_parks
            .values()
            .any(|record| record.phase != DirectoryParkRegistryPhase::Abandoned)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an externally owned park obligation",
        ));
    }
    if state
        .stages
        .values()
        .any(|record| record.carrier != StageCarrierState::Abandoned)
        || state.stage_creations.values().any(|record| {
            !matches!(
                record.phase,
                StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
            )
        })
        || state.directory_creations.values().any(|record| {
            !matches!(
                record.phase,
                DirectoryCreateEffectPhase::Abandoned
                    | DirectoryCreateEffectPhase::CleanupAttempted
                    | DirectoryCreateEffectPhase::UnclassifiedAbandoned
            )
        })
        || state
            .transients
            .values()
            .any(|record| record.phase != transient::TransientEffectPhase::Abandoned)
    {
        return Err(io::Error::new(
            io::ErrorKind::WouldBlock,
            "filesystem session still has an externally owned effect obligation",
        ));
    }
    Ok(())
}

#[derive(Clone)]
pub struct EffectOwner {
    state: Arc<EffectOwnerState>,
}

impl_redacted_debug!(EffectOwner);

#[must_use = "refused effect retention returns its linear carrier"]
pub struct EffectOwnerRetentionError<T> {
    error: Option<io::Error>,
    carrier: Option<T>,
}

impl<T> fmt::Debug for EffectOwnerRetentionError<T> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EffectOwnerRetentionError")
            .finish_non_exhaustive()
    }
}

impl<T> EffectOwnerRetentionError<T> {
    fn new(error: io::Error, carrier: T) -> Self {
        Self {
            error: Some(error),
            carrier: Some(carrier),
        }
    }

    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("effect retention failure retains its error")
    }

    pub fn into_parts(mut self) -> (io::Error, T) {
        (
            self.error
                .take()
                .expect("effect retention failure retains its error"),
            self.carrier
                .take()
                .expect("effect retention failure retains its carrier"),
        )
    }
}

impl<T> Drop for EffectOwnerRetentionError<T> {
    fn drop(&mut self) {
        if self.error.is_some() || self.carrier.is_some() {
            std::process::abort();
        }
    }
}

struct EffectOwnerState {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    anchor: Directory,
    effects: Mutex<EffectOwnerRecords>,
    #[cfg(test)]
    settlement_pause: Mutex<Option<EffectOwnerSettlementPause>>,
}

#[cfg(test)]
struct EffectOwnerSettlementPause {
    extracted: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

struct EffectOwnerRecords {
    next_id: u64,
    settling: bool,
    in_flight: usize,
    effects: BTreeMap<u64, OwnedEffect>,
}

type ReceiptLiveness = Arc<AtomicBool>;

fn receipt_is_live(live: &ReceiptLiveness) -> bool {
    // Receipt Drop publishes abandonment even while settlement owns the extracted record.
    live.load(Ordering::Acquire)
}

enum OwnedEffect {
    StageCreateCleanup(FileCreateObligation),
    DirectoryCreateCompletion(DirectoryCreateObligation),
    DirectoryCreatePreservation(DirectoryCreatePreservation),
    StageDiscard(StageDiscardObligation),
    FileParkRemoval(FileParkObligation),
    ParkedFileRemoval(ParkedFile),
    FileRemoval(FileRemovalObligation),
    FileParkRestore(FileParkObligation),
    ParkedFileRestore(ParkedFile),
    FileRestore(FileRestoreObligation),
    ParkedFilePreservation(ParkedFile),
    DirectoryParkRemoval(DirectoryParkObligation),
    ParkedDirectoryRemoval(ParkedDirectory),
    DirectoryRemoval(DirectoryRemovalObligation),
    ParkedDirectoryTreeRemoval(RetainedDirectoryTreeRemoval),
    DirectoryTreeRemoval(DirectoryTreeRemovalObligation),
    DirectoryParkRestore(DirectoryParkObligation),
    ParkedDirectoryRestore(ParkedDirectory),
    DirectoryRestore(DirectoryRestoreObligation),
    FilePromotion(OwnedFilePromotion),
    FileReplace(OwnedFileReplace),
    FileMove(OwnedFileMove),
    DirectoryMove(OwnedDirectoryMove),
}

enum OwnedFilePromotion {
    Pending {
        obligation: FilePromotionObligation,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: FilePromotionTerminal,
        receipt_live: ReceiptLiveness,
    },
}

enum FilePromotionTerminal {
    Applied(FileCapability),
    NoEffect(SealedStagedFile),
}

enum OwnedFileReplace {
    Pending {
        obligation: FileReplaceObligation,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: FileReplaceTerminal,
        receipt_live: ReceiptLiveness,
    },
}

enum FileReplaceTerminal {
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
}

enum OwnedFileMove {
    Pending {
        obligation: FileMoveObligation,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: FileMoveTerminal,
        receipt_live: ReceiptLiveness,
    },
}

enum FileMoveTerminal {
    Applied(FileCapability),
    NoEffect(FileCapability),
}

enum OwnedDirectoryMove {
    Pending {
        obligation: DirectoryMoveObligation,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: DirectoryMoveTerminal,
        receipt_live: ReceiptLiveness,
    },
}

enum DirectoryMoveTerminal {
    Applied(Directory),
    NoEffect(Directory),
}

#[must_use = "file promotion receipts must be claimed after explicit owner settlement"]
pub struct FilePromotionReceipt {
    owner: Arc<EffectOwnerState>,
    id: u64,
    live: ReceiptLiveness,
}

impl_redacted_debug!(FilePromotionReceipt);

#[must_use = "file promotion receipt outcomes must be handled"]
pub enum FilePromotionReceiptOutcome {
    Pending(FilePromotionReceipt),
    Applied(FileCapability),
    NoEffect(SealedStagedFile),
}

impl_redacted_debug!(FilePromotionReceiptOutcome);

#[must_use = "file replacement receipts must be claimed after explicit owner settlement"]
pub struct FileReplaceReceipt {
    owner: Arc<EffectOwnerState>,
    id: u64,
    live: ReceiptLiveness,
}

impl_redacted_debug!(FileReplaceReceipt);

#[must_use = "file replacement receipt outcomes must be handled"]
pub enum FileReplaceReceiptOutcome {
    Pending(FileReplaceReceipt),
    Replaced {
        current: FileCapability,
        displaced: Option<ParkedFile>,
    },
    NoEffect {
        staged: SealedStagedFile,
        destination: ReplaceDestination,
    },
}

impl_redacted_debug!(FileReplaceReceiptOutcome);

#[must_use = "file move receipts must be claimed after explicit owner settlement"]
pub struct FileMoveReceipt {
    owner: Arc<EffectOwnerState>,
    id: u64,
    live: ReceiptLiveness,
}

impl_redacted_debug!(FileMoveReceipt);

#[must_use = "file move receipt outcomes must be handled"]
pub enum FileMoveReceiptOutcome {
    Pending(FileMoveReceipt),
    Applied(FileCapability),
    NoEffect(FileCapability),
}

impl_redacted_debug!(FileMoveReceiptOutcome);

#[must_use = "directory move receipts must be claimed after explicit owner settlement"]
pub struct DirectoryMoveReceipt {
    owner: Arc<EffectOwnerState>,
    id: u64,
    live: ReceiptLiveness,
}

impl_redacted_debug!(DirectoryMoveReceipt);

#[must_use = "directory move receipt outcomes must be handled"]
pub enum DirectoryMoveReceiptOutcome {
    Pending(DirectoryMoveReceipt),
    Applied(Directory),
    NoEffect(Directory),
}

impl_redacted_debug!(DirectoryMoveReceiptOutcome);

impl EffectOwner {
    pub fn anchor_identity(&self) -> DirectoryIdentity {
        self.state.anchor.inner.identity
    }

    pub fn has_pending(&self) -> bool {
        let records = self
            .state
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        records.settling || records.in_flight != 0 || !records.effects.is_empty()
    }

    pub fn require_settled(&self) -> io::Result<()> {
        if self.has_pending() {
            Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect owner has unsettled or unclaimed effects",
            ))
        } else {
            Ok(())
        }
    }

    pub fn settle(&self) -> io::Result<()> {
        self.state.settle(false)
    }

    pub fn retain_stage_create_cleanup(
        &self,
        obligation: FileCreateObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileCreateObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                authority.stage_create_is_within(&obligation.token, anchor)
            },
            OwnedEffect::StageCreateCleanup,
        )
        .map(|_| ())
    }

    pub fn retain_directory_create_completion(
        &self,
        obligation: DirectoryCreateObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryCreateObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                authority.directory_create_is_within(&obligation.token, anchor)
            },
            OwnedEffect::DirectoryCreateCompletion,
        )
        .map(|_| ())
    }

    pub fn retain_directory_create_preservation(
        &self,
        preservation: DirectoryCreatePreservation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryCreatePreservation>> {
        self.retain(
            preservation,
            |authority, anchor, preservation| {
                authority.directory_create_is_within(&preservation.token, anchor)
            },
            OwnedEffect::DirectoryCreatePreservation,
        )
        .map(|_| ())
    }

    pub fn retain_stage_discard(
        &self,
        obligation: StageDiscardObligation,
    ) -> Result<(), EffectOwnerRetentionError<StageDiscardObligation>> {
        self.retain(
            obligation,
            |authority, anchor, obligation| {
                obligation
                    .token
                    .as_ref()
                    .is_some_and(|token| authority.stage_is_within(token, anchor))
            },
            OwnedEffect::StageDiscard,
        )
        .map(|_| ())
    }

    pub fn retain_file_park_removal(
        &self,
        obligation: FileParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| file_park_obligation_is_within(obligation, anchor),
            OwnedEffect::FileParkRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_file_removal(
        &self,
        parked: ParkedFile,
    ) -> Result<(), EffectOwnerRetentionError<ParkedFile>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedFileRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_file_removal(
        &self,
        obligation: FileRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::FileRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_file_park_restore(
        &self,
        obligation: FileParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| file_park_obligation_is_within(obligation, anchor),
            OwnedEffect::FileParkRestore,
        )
        .map(|_| ())
    }

    pub fn retain_parked_file_restore(
        &self,
        parked: ParkedFile,
    ) -> Result<(), EffectOwnerRetentionError<ParkedFile>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedFileRestore,
        )
        .map(|_| ())
    }

    pub fn retain_file_restore(
        &self,
        obligation: FileRestoreObligation,
    ) -> Result<(), EffectOwnerRetentionError<FileRestoreObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::FileRestore,
        )
        .map(|_| ())
    }

    pub fn retain_parked_file_preservation(
        &self,
        parked: ParkedFile,
    ) -> Result<(), EffectOwnerRetentionError<ParkedFile>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedFilePreservation,
        )
        .map(|_| ())
    }

    pub fn retain_directory_park_removal(
        &self,
        obligation: DirectoryParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation.parent.is_within(anchor)
                    && obligation
                        .directory
                        .as_ref()
                        .is_some_and(|directory| directory.is_within(anchor))
            },
            OwnedEffect::DirectoryParkRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_directory_removal(
        &self,
        parked: ParkedDirectory,
    ) -> Result<(), EffectOwnerRetentionError<ParkedDirectory>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedDirectoryRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_removal(
        &self,
        obligation: DirectoryRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::DirectoryRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_parked_directory_tree_removal(
        &self,
        retained: RetainedDirectoryTreeRemoval,
    ) -> Result<(), EffectOwnerRetentionError<RetainedDirectoryTreeRemoval>> {
        self.retain(
            retained,
            |_, anchor, retained| {
                retained
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::ParkedDirectoryTreeRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_tree_removal(
        &self,
        obligation: DirectoryTreeRemovalObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryTreeRemovalObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::DirectoryTreeRemoval,
        )
        .map(|_| ())
    }

    pub fn retain_directory_park_restore(
        &self,
        obligation: DirectoryParkObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryParkObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation.parent.is_within(anchor)
                    && obligation
                        .directory
                        .as_ref()
                        .is_some_and(|directory| directory.is_within(anchor))
            },
            OwnedEffect::DirectoryParkRestore,
        )
        .map(|_| ())
    }

    pub fn retain_parked_directory_restore(
        &self,
        parked: ParkedDirectory,
    ) -> Result<(), EffectOwnerRetentionError<ParkedDirectory>> {
        self.retain(
            parked,
            |_, anchor, parked| parked.parent.is_within(anchor),
            OwnedEffect::ParkedDirectoryRestore,
        )
        .map(|_| ())
    }

    pub fn retain_directory_restore(
        &self,
        obligation: DirectoryRestoreObligation,
    ) -> Result<(), EffectOwnerRetentionError<DirectoryRestoreObligation>> {
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .parked
                    .as_ref()
                    .is_some_and(|parked| parked.parent.is_within(anchor))
            },
            OwnedEffect::DirectoryRestore,
        )
        .map(|_| ())
    }

    pub fn retain_file_promotion(
        &self,
        obligation: FilePromotionObligation,
    ) -> Result<FilePromotionReceipt, EffectOwnerRetentionError<FilePromotionObligation>> {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation.retained.file.parent.is_within(anchor)
                    && obligation.destination.is_within(anchor)
            },
            move |obligation| {
                OwnedEffect::FilePromotion(OwnedFilePromotion::Pending {
                    obligation,
                    receipt_live: record_live,
                })
            },
        )
        .map(|id| FilePromotionReceipt {
            owner: self.state.clone(),
            id,
            live,
        })
    }

    pub fn retain_file_replace(
        &self,
        obligation: FileReplaceObligation,
    ) -> Result<FileReplaceReceipt, EffectOwnerRetentionError<FileReplaceObligation>> {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            obligation,
            |_, anchor, obligation| file_replace_obligation_is_within(obligation, anchor),
            move |obligation| {
                OwnedEffect::FileReplace(OwnedFileReplace::Pending {
                    obligation,
                    receipt_live: record_live,
                })
            },
        )
        .map(|id| FileReplaceReceipt {
            owner: self.state.clone(),
            id,
            live,
        })
    }

    pub fn retain_file_move(
        &self,
        obligation: FileMoveObligation,
    ) -> Result<FileMoveReceipt, EffectOwnerRetentionError<FileMoveObligation>> {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .file
                    .as_ref()
                    .is_some_and(|file| file.parent.is_within(anchor))
                    && obligation.destination.is_within(anchor)
            },
            move |obligation| {
                OwnedEffect::FileMove(OwnedFileMove::Pending {
                    obligation,
                    receipt_live: record_live,
                })
            },
        )
        .map(|id| FileMoveReceipt {
            owner: self.state.clone(),
            id,
            live,
        })
    }

    pub fn retain_directory_move(
        &self,
        obligation: DirectoryMoveObligation,
    ) -> Result<DirectoryMoveReceipt, EffectOwnerRetentionError<DirectoryMoveObligation>> {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .directory
                    .as_ref()
                    .is_some_and(|directory| directory.is_within(anchor))
                    && obligation.destination.is_within(anchor)
            },
            move |obligation| {
                OwnedEffect::DirectoryMove(OwnedDirectoryMove::Pending {
                    obligation,
                    receipt_live: record_live,
                })
            },
        )
        .map(|id| DirectoryMoveReceipt {
            owner: self.state.clone(),
            id,
            live,
        })
    }

    fn retain<T>(
        &self,
        carrier: T,
        validate: impl FnOnce(&Arc<CapabilityAuthority>, &Directory, &T) -> bool,
        wrap: impl FnOnce(T) -> OwnedEffect,
    ) -> Result<u64, EffectOwnerRetentionError<T>> {
        let Some(authority) = self.state.authority.upgrade() else {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        };
        let operation = match authority.enter_effect_retention(self.state.id) {
            Ok(operation) => operation,
            Err(error) => return Err(EffectOwnerRetentionError::new(error, carrier)),
        };
        if !validate(&authority, &self.state.anchor, &carrier) {
            return Err(EffectOwnerRetentionError::new(
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "filesystem effect lies outside its owner's anchored subtree",
                ),
                carrier,
            ));
        }
        authority.retain_effect_owner_record(&self.state, &operation, carrier, wrap)
    }
}

fn file_park_obligation_is_within(
    obligation: &FileParkObligation,
    anchor: &Directory,
) -> bool {
    obligation
        .request
        .as_ref()
        .is_some_and(|request| request.file.parent.is_within(anchor))
}

fn replace_destination_is_within(destination: &ReplaceDestination, anchor: &Directory) -> bool {
    match destination {
        ReplaceDestination::Vacant { parent, .. } => parent.is_within(anchor),
        ReplaceDestination::Existing(request) => request.file.parent.is_within(anchor),
    }
}

fn file_promotion_obligation_is_within(
    obligation: &FilePromotionObligation,
    anchor: &Directory,
) -> bool {
    obligation.retained.file.parent.is_within(anchor) && obligation.destination.is_within(anchor)
}

fn file_replace_obligation_is_within(
    obligation: &FileReplaceObligation,
    anchor: &Directory,
) -> bool {
    obligation.state.as_ref().is_some_and(|state| match state {
        FileReplaceObligationState::Parking { park, staged, .. } => {
            file_park_obligation_is_within(park, anchor)
                && staged.file.parent.is_within(anchor)
        }
        FileReplaceObligationState::Promoting {
            promotion,
            displaced,
            fallback,
            ..
        } => {
            file_promotion_obligation_is_within(promotion, anchor)
                && displaced
                    .as_ref()
                    .is_none_or(|parked| parked.parent.is_within(anchor))
                && replace_destination_is_within(fallback, anchor)
        }
        FileReplaceObligationState::RestoreParked { parked, staged, .. } => {
            parked.parent.is_within(anchor) && staged.file.parent.is_within(anchor)
        }
        FileReplaceObligationState::RestoreObligation {
            restore, staged, ..
        } => {
            restore
                .parked
                .as_ref()
                .is_some_and(|parked| parked.parent.is_within(anchor))
                && staged.file.parent.is_within(anchor)
        }
    })
}

impl EffectOwnerState {
    fn settle(self: &Arc<Self>, terminal: bool) -> io::Result<()> {
        {
            let records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if records.settling || records.in_flight != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect owner settlement is already in progress",
                ));
            }
            if records.effects.is_empty() {
                return Ok(());
            }
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        let _terminal_scope = if terminal {
            Some(TerminalEffectSettlementScope::begin(
                &authority,
                self.id,
            )?)
        } else {
            None
        };
        let operation = authority.enter_effect_settlement(self.id, terminal)?;
        let (pending, in_flight) = {
            let mut records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            if records.settling {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect owner settlement is already in progress",
                ));
            }
            if records.effects.is_empty() {
                return Ok(());
            }
            let in_flight = records.effects.len();
            records.settling = true;
            records.in_flight = in_flight;
            (std::mem::take(&mut records.effects), in_flight)
        };
        #[cfg(test)]
        self.pause_after_settlement_extraction();
        let mut settled = BTreeMap::new();
        let mut blocked = false;
        for (id, effect) in pending {
            if blocked {
                settled.insert(id, effect);
                continue;
            }
            if let Some(effect) = effect.settle() {
                blocked = effect.is_unresolved();
                settled.insert(id, effect);
            }
        }
        {
            let mut records = self
                .effects
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            assert!(records.settling);
            assert_eq!(records.in_flight, in_flight);
            for (id, effect) in settled {
                assert!(records.effects.insert(id, effect).is_none());
            }
            records.in_flight = 0;
            records.settling = false;
        }
        drop(_terminal_scope);
        drop(operation);
        authority.deactivate_effect_owner_if_empty(self);
        Ok(())
    }

    #[cfg(test)]
    fn pause_after_settlement_extraction(&self) {
        let pause = self
            .settlement_pause
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(pause) = pause {
            pause.extracted.wait();
            pause.resume.wait();
        }
    }

    fn has_domain_pending(&self) -> bool {
        self.effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .effects
            .values()
            .any(OwnedEffect::is_domain_pending)
    }

    fn take_for_terminal_disposal(&self) -> Vec<OwnedEffect> {
        let mut records = self
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        assert!(!records.settling && records.in_flight == 0);
        std::mem::take(&mut records.effects).into_values().collect()
    }
}

impl OwnedEffect {
    fn is_domain_pending(&self) -> bool {
        match self {
            Self::DirectoryCreateCompletion(_)
                | Self::DirectoryCreatePreservation(_)
                | Self::FileParkRestore(_)
                | Self::ParkedFileRestore(_)
                | Self::FileRestore(_)
                | Self::ParkedFilePreservation(_)
                | Self::DirectoryParkRestore(_)
                | Self::ParkedDirectoryRestore(_)
                | Self::DirectoryRestore(_)
                | Self::ParkedDirectoryTreeRemoval(_)
                | Self::DirectoryTreeRemoval(_)
                | Self::FilePromotion(OwnedFilePromotion::Pending { .. })
                | Self::FileReplace(OwnedFileReplace::Pending { .. })
                | Self::FileMove(OwnedFileMove::Pending { .. })
                | Self::DirectoryMove(OwnedDirectoryMove::Pending { .. }) => true,
            Self::FilePromotion(OwnedFilePromotion::Ready { receipt_live, .. })
            | Self::FileReplace(OwnedFileReplace::Ready { receipt_live, .. })
            | Self::FileMove(OwnedFileMove::Ready { receipt_live, .. })
            | Self::DirectoryMove(OwnedDirectoryMove::Ready { receipt_live, .. }) => {
                receipt_is_live(receipt_live)
            }
            _ => false,
        }
    }

    fn is_unresolved(&self) -> bool {
        match self {
            Self::FilePromotion(OwnedFilePromotion::Ready { .. })
            | Self::FileReplace(OwnedFileReplace::Ready { .. })
            | Self::FileMove(OwnedFileMove::Ready { .. })
            | Self::DirectoryMove(OwnedDirectoryMove::Ready { .. }) => false,
            _ => true,
        }
    }

    fn settle(self) -> Option<Self> {
        match self {
            Self::StageCreateCleanup(obligation) => match obligation.reconcile() {
                FileCreateResolution::Created(staged) => owned_stage_discard(staged.discard()),
                FileCreateResolution::Indeterminate(obligation) => {
                    Some(Self::StageCreateCleanup(obligation))
                }
            },
            Self::DirectoryCreateCompletion(obligation) => match obligation.reconcile() {
                DirectoryCreateResolution::Created(directory) => {
                    drop(directory);
                    None
                }
                DirectoryCreateResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryCreateCompletion(obligation))
                }
            },
            Self::DirectoryCreatePreservation(preservation) => preservation
                .acknowledge_preserved()
                .err()
                .map(Self::DirectoryCreatePreservation),
            Self::StageDiscard(obligation) => match obligation.reconcile() {
                StageDiscardResolution::Discarded => None,
                StageDiscardResolution::Indeterminate(obligation) => {
                    Some(Self::StageDiscard(obligation))
                }
            },
            Self::FileParkRemoval(obligation) => match obligation.reconcile() {
                FileParkResolution::Parked(parked) => owned_parked_file_removal(parked),
                FileParkResolution::NoEffect(request) => {
                    drop(request);
                    None
                }
                FileParkResolution::Indeterminate(obligation) => {
                    Some(Self::FileParkRemoval(obligation))
                }
            },
            Self::ParkedFileRemoval(parked) => owned_parked_file_removal(parked),
            Self::FileRemoval(obligation) => match obligation.reconcile() {
                FileRemovalResolution::Removed => None,
                FileRemovalResolution::NoEffect(parked) => owned_parked_file_removal(parked),
                FileRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::FileRemoval(obligation))
                }
            },
            Self::FileParkRestore(obligation) => match obligation.restore() {
                FileParkResolution::Parked(parked) => owned_parked_file_restore(parked),
                FileParkResolution::NoEffect(request) => {
                    drop(request);
                    None
                }
                FileParkResolution::Indeterminate(obligation) => {
                    Some(Self::FileParkRestore(obligation))
                }
            },
            Self::ParkedFileRestore(parked) => owned_parked_file_restore(parked),
            Self::FileRestore(obligation) => match obligation.reconcile() {
                FileRestoreResolution::Restored(file) => {
                    drop(file);
                    None
                }
                FileRestoreResolution::NoEffect(parked) => owned_parked_file_restore(parked),
                FileRestoreResolution::Indeterminate(obligation) => {
                    Some(Self::FileRestore(obligation))
                }
            },
            Self::ParkedFilePreservation(parked) => match parked.acknowledge_preserved() {
                Ok(()) => None,
                Err(failure) => Some(Self::ParkedFilePreservation(failure.into_parked())),
            },
            Self::DirectoryParkRemoval(obligation) => match obligation.reconcile() {
                DirectoryParkResolution::Parked(parked) => {
                    owned_parked_directory_removal(parked)
                }
                DirectoryParkResolution::NoEffect(directory) => {
                    drop(directory);
                    None
                }
                DirectoryParkResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryParkRemoval(obligation))
                }
            },
            Self::ParkedDirectoryRemoval(parked) => owned_parked_directory_removal(parked),
            Self::DirectoryRemoval(obligation) => match obligation.reconcile() {
                DirectoryRemovalResolution::Removed => None,
                DirectoryRemovalResolution::NoEffect(parked) => {
                    owned_parked_directory_removal(parked)
                }
                DirectoryRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryRemoval(obligation))
                }
            },
            Self::ParkedDirectoryTreeRemoval(parked) => {
                owned_parked_directory_tree_removal(parked)
            }
            Self::DirectoryTreeRemoval(obligation) => match obligation.reconcile() {
                DirectoryTreeRemovalResolution::Removed => None,
                DirectoryTreeRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryTreeRemoval(obligation))
                }
            },
            Self::DirectoryParkRestore(obligation) => match obligation.restore() {
                DirectoryParkResolution::Parked(parked) => {
                    owned_parked_directory_restore(parked)
                }
                DirectoryParkResolution::NoEffect(directory) => {
                    drop(directory);
                    None
                }
                DirectoryParkResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryParkRestore(obligation))
                }
            },
            Self::ParkedDirectoryRestore(parked) => owned_parked_directory_restore(parked),
            Self::DirectoryRestore(obligation) => match obligation.reconcile() {
                DirectoryRestoreResolution::Restored(directory) => {
                    drop(directory);
                    None
                }
                DirectoryRestoreResolution::NoEffect(parked) => {
                    owned_parked_directory_restore(parked)
                }
                DirectoryRestoreResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryRestore(obligation))
                }
            },
            Self::FilePromotion(owned) => settle_owned_file_promotion(owned),
            Self::FileReplace(owned) => settle_owned_file_replace(owned),
            Self::FileMove(owned) => settle_owned_file_move(owned),
            Self::DirectoryMove(owned) => settle_owned_directory_move(owned),
        }
    }
}

fn settle_owned_file_promotion(owned: OwnedFilePromotion) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedFilePromotion::Pending {
            obligation,
            receipt_live,
        } => match obligation.reconcile() {
            FilePromotionResolution::Applied(file) => OwnedFilePromotion::Ready {
                terminal: FilePromotionTerminal::Applied(file),
                receipt_live,
            },
            FilePromotionResolution::NoEffect(staged) => OwnedFilePromotion::Ready {
                terminal: FilePromotionTerminal::NoEffect(staged),
                receipt_live,
            },
            FilePromotionResolution::Indeterminate(obligation) => OwnedFilePromotion::Pending {
                obligation,
                receipt_live,
            },
        },
        ready => ready,
    };
    match owned {
        OwnedFilePromotion::Ready {
            terminal,
            receipt_live,
        } if !receipt_is_live(&receipt_live) => {
            drop(terminal);
            None
        }
        owned => Some(OwnedEffect::FilePromotion(owned)),
    }
}

fn settle_owned_file_replace(owned: OwnedFileReplace) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedFileReplace::Pending {
            obligation,
            receipt_live,
        } => match obligation.reconcile() {
            FileReplaceResolution::Replaced { current, displaced } => OwnedFileReplace::Ready {
                terminal: FileReplaceTerminal::Replaced { current, displaced },
                receipt_live,
            },
            FileReplaceResolution::NoEffect {
                staged,
                destination,
            } => OwnedFileReplace::Ready {
                terminal: FileReplaceTerminal::NoEffect {
                    staged,
                    destination,
                },
                receipt_live,
            },
            FileReplaceResolution::Indeterminate(obligation) => OwnedFileReplace::Pending {
                obligation,
                receipt_live,
            },
        },
        ready => ready,
    };
    match owned {
        OwnedFileReplace::Ready {
            terminal,
            receipt_live,
        } if !receipt_is_live(&receipt_live) => {
            drop(terminal);
            None
        }
        owned => Some(OwnedEffect::FileReplace(owned)),
    }
}

fn settle_owned_file_move(owned: OwnedFileMove) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedFileMove::Pending {
            obligation,
            receipt_live,
        } => match obligation.reconcile() {
            FileMoveResolution::Applied(file) => OwnedFileMove::Ready {
                terminal: FileMoveTerminal::Applied(file),
                receipt_live,
            },
            FileMoveResolution::NoEffect(file) => OwnedFileMove::Ready {
                terminal: FileMoveTerminal::NoEffect(file),
                receipt_live,
            },
            FileMoveResolution::Indeterminate(obligation) => OwnedFileMove::Pending {
                obligation,
                receipt_live,
            },
        },
        ready => ready,
    };
    match owned {
        OwnedFileMove::Ready {
            terminal,
            receipt_live,
        } if !receipt_is_live(&receipt_live) => {
            drop(terminal);
            None
        }
        owned => Some(OwnedEffect::FileMove(owned)),
    }
}

fn settle_owned_directory_move(owned: OwnedDirectoryMove) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedDirectoryMove::Pending {
            obligation,
            receipt_live,
        } => match obligation.reconcile() {
            DirectoryMoveResolution::Applied(directory) => OwnedDirectoryMove::Ready {
                terminal: DirectoryMoveTerminal::Applied(directory),
                receipt_live,
            },
            DirectoryMoveResolution::NoEffect(directory) => OwnedDirectoryMove::Ready {
                terminal: DirectoryMoveTerminal::NoEffect(directory),
                receipt_live,
            },
            DirectoryMoveResolution::Indeterminate(obligation) => OwnedDirectoryMove::Pending {
                obligation,
                receipt_live,
            },
        },
        ready => ready,
    };
    match owned {
        OwnedDirectoryMove::Ready {
            terminal,
            receipt_live,
        } if !receipt_is_live(&receipt_live) => {
            drop(terminal);
            None
        }
        owned => Some(OwnedEffect::DirectoryMove(owned)),
    }
}

fn owned_stage_discard(outcome: StageDiscardOutcome) -> Option<OwnedEffect> {
    match outcome {
        StageDiscardOutcome::Discarded => None,
        StageDiscardOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::StageDiscard(obligation))
        }
    }
}

fn owned_parked_file_removal(parked: ParkedFile) -> Option<OwnedEffect> {
    match parked.remove() {
        FileRemovalOutcome::Removed => None,
        FileRemovalOutcome::NoEffect { parked, .. } => {
            Some(OwnedEffect::ParkedFileRemoval(parked))
        }
        FileRemovalOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::FileRemoval(obligation))
        }
    }
}

fn owned_parked_file_restore(parked: ParkedFile) -> Option<OwnedEffect> {
    match parked.restore() {
        FileRestoreOutcome::Restored(file) => {
            drop(file);
            None
        }
        FileRestoreOutcome::NoEffect { parked, .. } => {
            Some(OwnedEffect::ParkedFileRestore(parked))
        }
        FileRestoreOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::FileRestore(obligation))
        }
    }
}

fn owned_parked_directory_removal(parked: ParkedDirectory) -> Option<OwnedEffect> {
    match parked.remove_empty() {
        DirectoryRemovalOutcome::Removed => None,
        DirectoryRemovalOutcome::NoEffect { parked, .. } => {
            Some(OwnedEffect::ParkedDirectoryRemoval(parked))
        }
        DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::DirectoryRemoval(obligation))
        }
    }
}

fn owned_parked_directory_tree_removal(
    retained: RetainedDirectoryTreeRemoval,
) -> Option<OwnedEffect> {
    match retained.retry() {
        DirectoryTreeRemovalOutcome::Removed => None,
        DirectoryTreeRemovalOutcome::Retained { retained, .. } => {
            Some(OwnedEffect::ParkedDirectoryTreeRemoval(retained))
        }
        DirectoryTreeRemovalOutcome::Indeterminate(obligation) => {
            Some(OwnedEffect::DirectoryTreeRemoval(obligation))
        }
    }
}

fn owned_parked_directory_restore(parked: ParkedDirectory) -> Option<OwnedEffect> {
    match parked.restore() {
        DirectoryRestoreOutcome::Restored(directory) => {
            drop(directory);
            None
        }
        DirectoryRestoreOutcome::NoEffect { parked, .. } => {
            Some(OwnedEffect::ParkedDirectoryRestore(parked))
        }
        DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
            Some(OwnedEffect::DirectoryRestore(obligation))
        }
    }
}

impl FilePromotionReceipt {
    pub fn claim(self) -> FilePromotionReceiptOutcome {
        let mut records = self
            .owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let effect = records.effects.remove(&self.id);
        match effect {
            Some(OwnedEffect::FilePromotion(OwnedFilePromotion::Ready {
                terminal,
                receipt_live,
            })) if Arc::ptr_eq(&receipt_live, &self.live) && receipt_is_live(&receipt_live) => {
                drop(records);
                deactivate_claimed_owner(&self.owner);
                match terminal {
                    FilePromotionTerminal::Applied(file) => {
                        FilePromotionReceiptOutcome::Applied(file)
                    }
                    FilePromotionTerminal::NoEffect(staged) => {
                        FilePromotionReceiptOutcome::NoEffect(staged)
                    }
                }
            }
            Some(effect) => {
                records.effects.insert(self.id, effect);
                drop(records);
                FilePromotionReceiptOutcome::Pending(self)
            }
            None => {
                drop(records);
                FilePromotionReceiptOutcome::Pending(self)
            }
        }
    }
}

impl FileReplaceReceipt {
    pub fn claim(self) -> FileReplaceReceiptOutcome {
        let mut records = self
            .owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let effect = records.effects.remove(&self.id);
        match effect {
            Some(OwnedEffect::FileReplace(OwnedFileReplace::Ready {
                terminal,
                receipt_live,
            })) if Arc::ptr_eq(&receipt_live, &self.live) && receipt_is_live(&receipt_live) => {
                drop(records);
                deactivate_claimed_owner(&self.owner);
                match terminal {
                    FileReplaceTerminal::Replaced { current, displaced } => {
                        FileReplaceReceiptOutcome::Replaced { current, displaced }
                    }
                    FileReplaceTerminal::NoEffect {
                        staged,
                        destination,
                    } => FileReplaceReceiptOutcome::NoEffect {
                        staged,
                        destination,
                    },
                }
            }
            Some(effect) => {
                records.effects.insert(self.id, effect);
                drop(records);
                FileReplaceReceiptOutcome::Pending(self)
            }
            None => {
                drop(records);
                FileReplaceReceiptOutcome::Pending(self)
            }
        }
    }
}

impl FileMoveReceipt {
    pub fn claim(self) -> FileMoveReceiptOutcome {
        let mut records = self
            .owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let effect = records.effects.remove(&self.id);
        match effect {
            Some(OwnedEffect::FileMove(OwnedFileMove::Ready {
                terminal,
                receipt_live,
            })) if Arc::ptr_eq(&receipt_live, &self.live) && receipt_is_live(&receipt_live) => {
                drop(records);
                deactivate_claimed_owner(&self.owner);
                match terminal {
                    FileMoveTerminal::Applied(file) => FileMoveReceiptOutcome::Applied(file),
                    FileMoveTerminal::NoEffect(file) => FileMoveReceiptOutcome::NoEffect(file),
                }
            }
            Some(effect) => {
                records.effects.insert(self.id, effect);
                drop(records);
                FileMoveReceiptOutcome::Pending(self)
            }
            None => {
                drop(records);
                FileMoveReceiptOutcome::Pending(self)
            }
        }
    }
}

impl DirectoryMoveReceipt {
    pub fn claim(self) -> DirectoryMoveReceiptOutcome {
        let mut records = self
            .owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let effect = records.effects.remove(&self.id);
        match effect {
            Some(OwnedEffect::DirectoryMove(OwnedDirectoryMove::Ready {
                terminal,
                receipt_live,
            })) if Arc::ptr_eq(&receipt_live, &self.live) && receipt_is_live(&receipt_live) => {
                drop(records);
                deactivate_claimed_owner(&self.owner);
                match terminal {
                    DirectoryMoveTerminal::Applied(directory) => {
                        DirectoryMoveReceiptOutcome::Applied(directory)
                    }
                    DirectoryMoveTerminal::NoEffect(directory) => {
                        DirectoryMoveReceiptOutcome::NoEffect(directory)
                    }
                }
            }
            Some(effect) => {
                records.effects.insert(self.id, effect);
                drop(records);
                DirectoryMoveReceiptOutcome::Pending(self)
            }
            None => {
                drop(records);
                DirectoryMoveReceiptOutcome::Pending(self)
            }
        }
    }
}

impl Drop for FilePromotionReceipt {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

impl Drop for FileReplaceReceipt {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

impl Drop for FileMoveReceipt {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

impl Drop for DirectoryMoveReceipt {
    fn drop(&mut self) {
        self.live.store(false, Ordering::Release);
    }
}

fn deactivate_claimed_owner(owner: &Arc<EffectOwnerState>) {
    if let Some(authority) = owner.authority.upgrade() {
        authority.deactivate_effect_owner_if_empty(owner);
    }
}

struct CapabilityAuthority {
    operations: Mutex<OperationState>,
    session_nonce: [u8; 16],
    root: platform::RootGuard,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageRegistryPhase {
    Writing,
    Sealed,
    CleanupAttempted,
    PromotionAttempted,
    Unresolved,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageCarrierState {
    Live,
    Abandoned,
}

struct StageRecord {
    parent: Directory,
    name: LeafName,
    identity: platform::Identity,
    cleanup: platform::FileCleanupHandle,
    phase: StageRegistryPhase,
    carrier: StageCarrierState,
    destination: Option<NamespaceLeaf>,
}

struct NamespaceLeaf {
    parent: Directory,
    name: LeafName,
}

struct MoveEffectRecord {
    source: NamespaceLeaf,
    destination: NamespaceLeaf,
    moved_directory: Option<platform::Identity>,
}

fn namespace_leaf_matches(
    leaf: &NamespaceLeaf,
    directory: &Directory,
    name: &LeafName,
) -> bool {
    leaf.parent.inner.identity == directory.inner.identity
        && leaf_names_equivalent(leaf.name.as_os_str(), name.as_os_str())
}

fn directory_has_physical_ancestor(
    directory: &Directory,
    ancestor: platform::Identity,
) -> bool {
    let mut current = directory;
    loop {
        if current.inner.identity.physical == ancestor {
            return true;
        }
        let Some(parent) = current.inner.parent.as_ref() else {
            return false;
        };
        current = &parent.directory;
    }
}

fn move_conflicts_with_transient(
    movement: &MoveEffectRecord,
    directory: &Directory,
    name: &LeafName,
) -> bool {
    namespace_leaf_matches(&movement.source, directory, name)
        || namespace_leaf_matches(&movement.destination, directory, name)
        || movement
            .moved_directory
            .is_some_and(|identity| directory_has_physical_ancestor(directory, identity))
}

struct StageToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StageCreatePhase {
    Reserved,
    Applied,
    Abandoned,
    CleanupAttempted,
}

struct StageCreateRecord {
    parent: Directory,
    name: LeafName,
    created: Option<File>,
    cleanup: Option<platform::FileCleanupHandle>,
    identity: Option<platform::Identity>,
    phase: StageCreatePhase,
}

struct StageCreateToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

impl_redacted_debug!(StageCreateToken);

struct StageCreateRecordGuard {
    authority: Arc<CapabilityAuthority>,
    id: u64,
    record: Option<StageCreateRecord>,
}

impl StageCreateRecordGuard {
    fn record(&self) -> &StageCreateRecord {
        self.record.as_ref().expect("stage create guard retains record")
    }

    fn record_mut(&mut self) -> &mut StageCreateRecord {
        self.record.as_mut().expect("stage create guard retains record")
    }

    fn disarm(mut self, token: &mut StageCreateToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record.take();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.release_effect(operation);
        token.armed = false;
    }

    fn transfer(mut self, token: &mut StageCreateToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record.take();
        token.armed = false;
    }
}

impl Drop for StageCreateRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(state.stage_creations.insert(self.id, record).is_none());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryCreateEffectPhase {
    Reserved,
    Applied,
    Abandoned,
    CleanupAttempted,
    CreatedUnclassified,
    UnclassifiedAbandoned,
    UnclassifiedRecovery,
    CreatedUnclassifiedResetPending,
}

struct DirectoryCreateEffectRecord {
    parent: Directory,
    name: LeafName,
    created: Option<platform::DirectoryHandle>,
    cleanup: Option<platform::DirectoryCleanupHandle>,
    identity: Option<platform::Identity>,
    phase: DirectoryCreateEffectPhase,
}

struct DirectoryCreateEffectToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

impl_redacted_debug!(DirectoryCreateEffectToken);

struct MoveEffectToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

impl_redacted_debug!(MoveEffectToken);

impl MoveEffectToken {
    fn reserve(
        authority: &Arc<CapabilityAuthority>,
        operation: &CapabilityOperation,
        source: NamespaceLeaf,
        destination: NamespaceLeaf,
        moved_directory: Option<platform::Identity>,
    ) -> io::Result<Self> {
        if !Arc::ptr_eq(authority, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = authority.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        let record = MoveEffectRecord {
            source,
            destination,
            moved_directory,
        };
        if state.transients.values().any(|transient| {
            move_conflicts_with_transient(
                &record,
                &transient.directory,
                &transient.destination,
            )
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem move conflicts with an unsettled transient effect",
            ));
        }
        let id = state.reserve_move_effect(record)?;
        Ok(Self {
            id,
            authority: Arc::downgrade(authority),
            armed: true,
        })
    }

    fn settle(&mut self, operation: &CapabilityOperation) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
        if !Arc::ptr_eq(&authority, &operation.authority) {
            return Err(stale_capability());
        }
        authority.release_move_effect(self.id, operation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for MoveEffectToken {
    fn drop(&mut self) {
        if self.armed {
            std::process::abort();
        }
    }
}

struct DirectoryCreateEffectGuard {
    authority: Arc<CapabilityAuthority>,
    id: u64,
    record: Option<DirectoryCreateEffectRecord>,
}

impl DirectoryCreateEffectGuard {
    fn record(&self) -> &DirectoryCreateEffectRecord {
        self.record
            .as_ref()
            .expect("directory create guard retains record")
    }

    fn record_mut(&mut self) -> &mut DirectoryCreateEffectRecord {
        self.record
            .as_mut()
            .expect("directory create guard retains record")
    }

    fn disarm(
        mut self,
        token: &mut DirectoryCreateEffectToken,
        operation: &CapabilityOperation,
    ) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record.take();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for DirectoryCreateEffectGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(state.directory_creations.insert(self.id, record).is_none());
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileParkRegistryPhase {
    Reserved,
    Live,
    Abandoned,
}

#[derive(Clone, Eq, Hash, PartialEq)]
struct ParkRegistryKey {
    parent: DirectoryIdentity,
    original_name: LeafName,
    park_name: LeafName,
    identity: platform::Identity,
}

impl ParkRegistryKey {
    fn new(
        parent: &Directory,
        original_name: &LeafName,
        park_name: &LeafName,
        identity: platform::Identity,
    ) -> Self {
        Self {
            parent: parent.inner.identity,
            original_name: original_name.clone(),
            park_name: park_name.clone(),
            identity,
        }
    }

    fn conflicts_with(&self, other: &Self) -> bool {
        let same_leaf = |first: &LeafName, second: &LeafName| {
            platform::leaf_names_equal(first.as_os_str(), second.as_os_str())
        };
        (self.parent == other.parent
            && (same_leaf(&self.original_name, &other.original_name)
                || same_leaf(&self.original_name, &other.park_name)
                || same_leaf(&self.park_name, &other.original_name)
                || same_leaf(&self.park_name, &other.park_name)))
            || self.identity == other.identity
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ParkRegistryOwner {
    File(u64),
    Directory(u64),
}

struct FileParkRegistryRecord {
    parent: Directory,
    original_name: LeafName,
    name: LeafName,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
    expected_digest: Option<[u8; 32]>,
    cleanup: platform::FileCleanupHandle,
    phase: FileParkRegistryPhase,
}

impl FileParkRegistryRecord {
    fn key(&self) -> ParkRegistryKey {
        ParkRegistryKey {
            parent: self.parent.inner.identity,
            original_name: self.original_name.clone(),
            park_name: self.name.clone(),
            identity: self.identity,
        }
    }
}

struct FileParkRegistryToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DirectoryParkRegistryPhase {
    Reserved,
    Live,
    Abandoned,
}

struct DirectoryParkRegistryRecord {
    parent: Directory,
    original_name: LeafName,
    name: LeafName,
    identity: platform::Identity,
    cleanup: platform::DirectoryCleanupHandle,
    phase: DirectoryParkRegistryPhase,
}

impl DirectoryParkRegistryRecord {
    fn key(&self) -> ParkRegistryKey {
        ParkRegistryKey {
            parent: self.parent.inner.identity,
            original_name: self.original_name.clone(),
            park_name: self.name.clone(),
            identity: self.identity,
        }
    }
}

struct DirectoryParkRegistryToken {
    id: u64,
    authority: Weak<CapabilityAuthority>,
    armed: bool,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum DrainRecoveryParkId {
    File(u64),
    Directory(u64),
    DirectoryCreate(u64),
}

struct DrainRecoveryPermit {
    authority: Arc<CapabilityAuthority>,
    park_token_id: DrainRecoveryParkId,
}

impl_redacted_debug!(DrainRecoveryPermit);

impl_redacted_debug!(FileParkRegistryToken);
impl_redacted_debug!(DirectoryParkRegistryToken);

struct FileParkRecordGuard {
    authority: Arc<CapabilityAuthority>,
    id: u64,
    record: Option<FileParkRegistryRecord>,
}

impl FileParkRecordGuard {
    fn record(&self) -> &FileParkRegistryRecord {
        self.record.as_ref().expect("file park guard retains record")
    }

    fn record_mut(&mut self) -> &mut FileParkRegistryRecord {
        self.record.as_mut().expect("file park guard retains record")
    }

    fn disarm(mut self, token: &mut FileParkRegistryToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        let record = self.record.take().expect("file park guard retains record");
        let key = record.key();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            state.park_owners.remove(&key),
            Some(ParkRegistryOwner::File(self.id))
        );
        assert!(state.file_parks_checked_out > 0);
        state.file_parks_checked_out -= 1;
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for FileParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let key = record.key();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            state.park_owners.get(&key),
            Some(&ParkRegistryOwner::File(self.id))
        );
        assert!(state.file_parks_checked_out > 0);
        state.file_parks_checked_out -= 1;
        assert!(state.file_parks.insert(self.id, record).is_none());
    }
}

struct DirectoryParkRecordGuard {
    authority: Arc<CapabilityAuthority>,
    id: u64,
    record: Option<DirectoryParkRegistryRecord>,
}

enum SessionDrainSettlement {
    Ready,
    Pending,
    Recovery {
        recovery: SessionDrainRecoveryState,
        permits: Vec<DrainRecoveryPermit>,
    },
}

impl DirectoryParkRecordGuard {
    fn record(&self) -> &DirectoryParkRegistryRecord {
        self.record
            .as_ref()
            .expect("directory park guard retains record")
    }

    fn record_mut(&mut self) -> &mut DirectoryParkRegistryRecord {
        self.record
            .as_mut()
            .expect("directory park guard retains record")
    }

    fn disarm(
        mut self,
        token: &mut DirectoryParkRegistryToken,
        operation: &CapabilityOperation,
    ) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        let record = self
            .record
            .take()
            .expect("directory park guard retains record");
        let key = record.key();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            state.park_owners.remove(&key),
            Some(ParkRegistryOwner::Directory(self.id))
        );
        assert!(state.directory_parks_checked_out > 0);
        state.directory_parks_checked_out -= 1;
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for DirectoryParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let key = record.key();
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert_eq!(
            state.park_owners.get(&key),
            Some(&ParkRegistryOwner::Directory(self.id))
        );
        assert!(state.directory_parks_checked_out > 0);
        state.directory_parks_checked_out -= 1;
        assert!(state.directory_parks.insert(self.id, record).is_none());
    }
}

impl fmt::Debug for StageToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("StageToken").finish_non_exhaustive()
    }
}

impl CapabilityAuthority {
    fn validate_retained_process_image_outside_root(&self) -> io::Result<()> {
        platform::validate_process_image_outside_root(&self.process_image, &self.root)
    }

    fn release_move_effect(
        self: &Arc<Self>,
        id: u64,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if (state.phase != AUTHORITY_LIVE
            && !(state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(self)))
            || state.active == 0
            || state.moves.remove(&id).is_none()
        {
            return Err(stale_capability());
        }
        state.release_effect(operation);
        Ok(())
    }

    fn enter(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE
            && !(state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_effect_settlement(
        self: &Arc<Self>,
        owner_id: u64,
        terminal: bool,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let expected_phase = if terminal {
            AUTHORITY_QUIESCING
        } else {
            AUTHORITY_LIVE
        };
        if state.phase != expected_phase
            || (terminal && !terminal_effect_settlement_admits_owner(self, owner_id))
            || !state
                .effect_owner_handles
                .get(&owner_id)
                .is_some_and(|owner| owner.strong_count() > 0)
        {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_effect_retention(
        self: &Arc<Self>,
        owner_id: u64,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE || !state.effect_owner_handles.contains_key(&owner_id)
        {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn create_effect_owner(
        self: &Arc<Self>,
        anchor: Directory,
        operation: &CapabilityOperation,
    ) -> io::Result<EffectOwner> {
        if !Arc::ptr_eq(self, &operation.authority)
            || anchor.inner.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        anchor.validate(operation)?;
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        state
            .effect_owner_handles
            .retain(|_, owner| owner.strong_count() > 0);
        if state.effect_owner_handles.len() >= MAX_EFFECT_OWNERS {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect-owner capacity is exhausted",
            ));
        }
        let id = state.next_effect_owner_id;
        state.next_effect_owner_id = state
            .next_effect_owner_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem effect-owner id overflowed"))?;
        let owner = Arc::new(EffectOwnerState {
            id,
            authority: Arc::downgrade(self),
            anchor,
            effects: Mutex::new(EffectOwnerRecords {
                next_id: 1,
                settling: false,
                in_flight: 0,
                effects: BTreeMap::new(),
            }),
            #[cfg(test)]
            settlement_pause: Mutex::new(None),
        });
        state.effect_owner_handles.insert(id, Arc::downgrade(&owner));
        Ok(EffectOwner { state: owner })
    }

    fn retain_effect_owner_record<T>(
        self: &Arc<Self>,
        owner: &Arc<EffectOwnerState>,
        operation: &CapabilityOperation,
        carrier: T,
        wrap: impl FnOnce(T) -> OwnedEffect,
    ) -> Result<u64, EffectOwnerRetentionError<T>> {
        if !Arc::ptr_eq(self, &operation.authority)
            || owner.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        }
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase != AUTHORITY_LIVE
            || !state
                .effect_owner_handles
                .get(&owner.id)
                .is_some_and(|registered| Weak::ptr_eq(registered, &Arc::downgrade(owner)))
        {
            return Err(EffectOwnerRetentionError::new(stale_capability(), carrier));
        }
        let mut records = owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let retained = records
            .effects
            .len()
            .checked_add(records.in_flight)
            .unwrap_or(usize::MAX);
        if retained >= MAX_EFFECTS_PER_OWNER {
            return Err(EffectOwnerRetentionError::new(
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect-owner capacity is exhausted",
                ),
                carrier,
            ));
        }
        let id = records.next_id;
        let Some(next_id) = records.next_id.checked_add(1) else {
            return Err(EffectOwnerRetentionError::new(
                io::Error::other("filesystem owned-effect id overflowed"),
                carrier,
            ));
        };
        records.next_id = next_id;
        assert!(records.effects.insert(id, wrap(carrier)).is_none());
        state
            .active_effect_owners
            .entry(owner.id)
            .or_insert_with(|| owner.clone());
        Ok(id)
    }

    fn deactivate_effect_owner_if_empty(&self, owner: &Arc<EffectOwnerState>) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let records = owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let empty = !records.settling && records.in_flight == 0 && records.effects.is_empty();
        if empty {
            state.active_effect_owners.remove(&owner.id);
        }
    }

    fn stage_create_is_within(&self, token: &StageCreateToken, anchor: &Directory) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .stage_creations
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    fn directory_create_is_within(
        &self,
        token: &DirectoryCreateEffectToken,
        anchor: &Directory,
    ) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .directory_creations
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    fn stage_is_within(&self, token: &StageToken, anchor: &Directory) -> bool {
        token.armed
            && std::ptr::eq(token.authority.as_ptr(), self)
            && self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .stages
                .get(&token.id)
                .is_some_and(|record| record.parent.is_within(anchor))
    }

    fn enter_file_park(
        self: &Arc<Self>,
        token: &FileParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && state
                .file_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == FileParkRegistryPhase::Live);
        let phase_admitted = state.phase == AUTHORITY_LIVE
            || (state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(self));
        if !admitted || !phase_admitted {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_file_park_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &FileParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::File(token.id)
            && state
                .file_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == FileParkRegistryPhase::Live);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_directory_park(
        self: &Arc<Self>,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && state
                .directory_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == DirectoryParkRegistryPhase::Live);
        let phase_admitted = state.phase == AUTHORITY_LIVE
            || (state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(self));
        if !admitted || !phase_admitted {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_directory_park_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::Directory(token.id)
            && state
                .directory_parks
                .get(&token.id)
                .is_some_and(|record| record.phase == DirectoryParkRegistryPhase::Live);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn enter_directory_create_recovery(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &DirectoryCreateEffectToken,
    ) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::DirectoryCreate(token.id)
            && state.directory_creations.get(&token.id).is_some_and(|record| {
                record.phase == DirectoryCreateEffectPhase::UnclassifiedRecovery
            });
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        let operation = CapabilityOperation {
            authority: self.clone(),
        };
        drop(state);
        platform::validate_lease(&self.lease)?;
        platform::validate_root(&self.root)?;
        Ok(operation)
    }

    fn transfer_unclassified_directory_create_to_reset(
        self: &Arc<Self>,
        permit: &DrainRecoveryPermit,
        token: &mut DirectoryCreateEffectToken,
    ) -> io::Result<()> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let admitted = Arc::ptr_eq(self, &permit.authority)
            && token.armed
            && token.authority.as_ptr() == Arc::as_ptr(self)
            && permit.park_token_id == DrainRecoveryParkId::DirectoryCreate(token.id);
        if !admitted || state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .filter(|record| {
                record.phase == DirectoryCreateEffectPhase::UnclassifiedRecovery
            })
            .ok_or_else(stale_capability)?;
        if !record.parent.is_managed_root_descendant() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external directory creation cannot transfer to app-root reset",
            ));
        }
        record.phase = DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending;
        token.armed = false;
        Ok(())
    }

    fn cancel_reset_pending_directory_creates(&self) -> io::Result<()> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        for record in state.directory_creations.values_mut() {
            if record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending {
                record.phase = DirectoryCreateEffectPhase::UnclassifiedAbandoned;
            }
        }
        Ok(())
    }

    fn enter_reset_operation(self: &Arc<Self>) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_RESETTING || state.active != 0 {
            return Err(stale_capability());
        }
        state.active = state
            .active
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem capability operation count overflowed"))?;
        Ok(CapabilityOperation {
            authority: self.clone(),
        })
    }

    fn has_reset_pending_directory_creates(&self) -> bool {
        let state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state.directory_creations.values().any(|record| {
            record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
        })
    }

    fn directory_create_is_external(&self, token: &DirectoryCreateEffectToken) -> bool {
        let state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        state
            .directory_creations
            .get(&token.id)
            .is_some_and(|record| !record.parent.is_managed_root_descendant())
    }

    fn retire_reset_pending_directory_creates(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_RESETTING || state.active == 0 {
            return Err(stale_capability());
        }
        let ids = state
            .directory_creations
            .iter()
            .filter_map(|(id, record)| {
                (record.phase
                    == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending)
                    .then_some(*id)
            })
            .collect::<Vec<_>>();
        for id in ids {
            state
                .directory_creations
                .remove(&id)
                .expect("reset-pending directory create remains registered");
            state.release_effect(operation);
        }
        Ok(())
    }

    fn identity(&self, physical: platform::Identity) -> DirectoryIdentity {
        DirectoryIdentity {
            session: self.session_nonce,
            physical,
        }
    }

    fn ensure_leaf_not_preserved_unclassified(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.directory_creations.values().any(|record| {
            record.parent.inner.identity == parent.inner.identity
                && record.name == *name
                && matches!(
                    record.phase,
                    DirectoryCreateEffectPhase::CreatedUnclassified
                        | DirectoryCreateEffectPhase::UnclassifiedAbandoned
                        | DirectoryCreateEffectPhase::UnclassifiedRecovery
                )
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory name is retained as an unclassified creation",
            ));
        }
        Ok(())
    }

    fn ensure_leaf_not_transient_reserved(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if transient::transient_leaf_is_reserved(&state, parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file name is reserved by an unsettled transient effect",
            ));
        }
        Ok(())
    }

    fn register_stage_record(
        self: &Arc<Self>,
        parent: Directory,
        name: LeafName,
        identity: platform::Identity,
        cleanup: platform::FileCleanupHandle,
        operation: &CapabilityOperation,
    ) -> io::Result<StageToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, &parent, &name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by a transient destination",
            ));
        }
        let id = state.next_stage_id;
        state.next_stage_id = state
            .next_stage_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("stage registry identity overflowed"))?;
        debug_assert!(state.outstanding_effects > 0);
        state.stages.insert(
            id,
            StageRecord {
                parent,
                name,
                identity,
                cleanup,
                phase: StageRegistryPhase::Writing,
                carrier: StageCarrierState::Live,
                destination: None,
            },
        );
        Ok(StageToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    fn reserve_stage_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<StageCreateToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by a transient destination",
            ));
        }
        let id = state.next_stage_create_id;
        state.next_stage_create_id = state
            .next_stage_create_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("stage create registry identity overflowed"))?;
        state.reserve_effect()?;
        state.stage_creations.insert(
            id,
            StageCreateRecord {
                parent: parent.clone(),
                name: name.clone(),
                created: None,
                cleanup: None,
                identity: None,
                phase: StageCreatePhase::Reserved,
            },
        );
        Ok(StageCreateToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    fn attach_stage_create(&self, token: &StageCreateToken, created: File) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .stage_creations
            .get_mut(&token.id)
            .expect("admitted stage create reservation remains registered");
        record.created = Some(created);
        record.phase = StageCreatePhase::Applied;
    }

    fn take_stage_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &StageCreateToken,
    ) -> io::Result<StageCreateRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let record = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?
            .stage_creations
            .remove(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
            })?;
        Ok(StageCreateRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    fn abandon_stage_create(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.stage_creations.get_mut(&id) {
            record.phase = StageCreatePhase::Abandoned;
        }
    }

    fn reserve_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        name: &LeafName,
    ) -> io::Result<DirectoryCreateEffectToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory name is reserved by a transient destination",
            ));
        }
        let id = state.next_directory_create_id;
        state.next_directory_create_id = state
            .next_directory_create_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("directory create registry identity overflowed"))?;
        state.reserve_effect()?;
        state.directory_creations.insert(
            id,
            DirectoryCreateEffectRecord {
                parent: parent.clone(),
                name: name.clone(),
                created: None,
                cleanup: None,
                identity: None,
                phase: DirectoryCreateEffectPhase::Reserved,
            },
        );
        Ok(DirectoryCreateEffectToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    fn attach_directory_create(
        &self,
        token: &DirectoryCreateEffectToken,
        created: platform::DirectoryHandle,
    ) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .expect("admitted directory create reservation remains registered");
        record.created = Some(created);
        record.phase = DirectoryCreateEffectPhase::Applied;
    }

    fn mark_directory_create_unclassified(&self, token: &DirectoryCreateEffectToken) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .expect("admitted directory create reservation remains registered");
        record.phase = DirectoryCreateEffectPhase::CreatedUnclassified;
    }

    fn acknowledge_unclassified_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut DirectoryCreateEffectToken,
    ) -> io::Result<()> {
        let guard = self.take_directory_create(operation, token)?;
        if !matches!(
            guard.record().phase,
            DirectoryCreateEffectPhase::CreatedUnclassified
                | DirectoryCreateEffectPhase::UnclassifiedRecovery
        ) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory creation is not an unclassified preservation",
            ));
        }
        guard.disarm(token, operation);
        Ok(())
    }

    fn take_directory_create(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &DirectoryCreateEffectToken,
    ) -> io::Result<DirectoryCreateEffectGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let record = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?
            .directory_creations
            .remove(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
        Ok(DirectoryCreateEffectGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    fn abandon_directory_create(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.directory_creations.get_mut(&id) {
            record.phase = match record.phase {
                DirectoryCreateEffectPhase::CreatedUnclassified
                | DirectoryCreateEffectPhase::UnclassifiedRecovery => {
                    DirectoryCreateEffectPhase::UnclassifiedAbandoned
                }
                phase => phase,
            };
            if matches!(
                record.phase,
                DirectoryCreateEffectPhase::Reserved | DirectoryCreateEffectPhase::Applied
            ) {
                record.phase = DirectoryCreateEffectPhase::Abandoned;
            }
        }
    }

    fn ensure_park_available(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        key: &ParkRegistryKey,
    ) -> io::Result<()> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if state.park_conflicts(key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "park ownership is already retained",
            ));
        }
        Ok(())
    }

    fn reserve_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        request: &FileParkRequest,
        park_name: LeafName,
        cleanup: platform::FileCleanupHandle,
    ) -> io::Result<FileParkRegistryToken> {
        self.register_file_park(
            operation,
            &request.file.parent,
            request.file.name.clone(),
            park_name,
            request.file.identity,
            request.expected.revision.size,
            request.expected.revision.stamp,
            Some(request.expected.sha256),
            cleanup,
            FileParkRegistryPhase::Reserved,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        identity: platform::Identity,
        size: u64,
        stamp: platform::FileStamp,
        expected_digest: Option<[u8; 32]>,
        cleanup: platform::FileCleanupHandle,
        phase: FileParkRegistryPhase,
    ) -> io::Result<FileParkRegistryToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let key = ParkRegistryKey::new(parent, &original_name, &park_name, identity);
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, parent, &original_name)
            || transient::transient_leaf_is_reserved(&state, parent, &park_name)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "file park name is reserved by a transient destination",
            ));
        }
        if state.park_conflicts(&key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "file park ownership is already retained",
            ));
        }
        let id = state.next_file_park_id;
        state.next_file_park_id = state
            .next_file_park_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("file park registry identity overflowed"))?;
        state.reserve_effect()?;
        assert!(state
            .file_parks
            .insert(
                id,
                FileParkRegistryRecord {
                    parent: parent.clone(),
                    original_name,
                    name: park_name,
                    identity,
                    size,
                    stamp,
                    expected_digest,
                    cleanup,
                    phase,
                },
            )
            .is_none());
        assert!(
            state
                .park_owners
                .insert(key, ParkRegistryOwner::File(id))
                .is_none()
        );
        Ok(FileParkRegistryToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    fn take_file_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &FileParkRegistryToken,
    ) -> io::Result<FileParkRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let checked_out = state
            .file_parks_checked_out
            .checked_add(1)
            .ok_or_else(|| io::Error::other("checked-out file park count overflowed"))?;
        let key = state
            .file_parks
            .get(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?
            .key();
        if state.park_owners.get(&key) != Some(&ParkRegistryOwner::File(token.id)) {
            return Err(io::Error::other(
                "file park ownership index is inconsistent",
            ));
        }
        let record = state
            .file_parks
            .remove(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        state.file_parks_checked_out = checked_out;
        drop(state);
        Ok(FileParkRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    fn rollback_file_park_registration(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut FileParkRegistryToken,
    ) -> io::Result<()> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let key = state
            .file_parks
            .get(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?
            .key();
        if state.park_owners.get(&key) != Some(&ParkRegistryOwner::File(token.id)) {
            return Err(io::Error::other(
                "file park ownership index is inconsistent",
            ));
        }
        let record = state
            .file_parks
            .remove(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        let removed = state
            .park_owners
            .remove(&record.key())
            .expect("prevalidated file park owner remains registered");
        assert_eq!(removed, ParkRegistryOwner::File(token.id));
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    fn reserve_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        directory: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        cleanup: platform::DirectoryCleanupHandle,
    ) -> io::Result<DirectoryParkRegistryToken> {
        self.register_directory_park(
            operation,
            parent,
            original_name,
            park_name,
            directory.inner.identity.physical,
            cleanup,
            DirectoryParkRegistryPhase::Reserved,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn register_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        parent: &Directory,
        original_name: LeafName,
        park_name: LeafName,
        identity: platform::Identity,
        cleanup: platform::DirectoryCleanupHandle,
        phase: DirectoryParkRegistryPhase,
    ) -> io::Result<DirectoryParkRegistryToken> {
        if !Arc::ptr_eq(self, &operation.authority) {
            return Err(stale_capability());
        }
        let key = ParkRegistryKey::new(parent, &original_name, &park_name, identity);
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, parent, &original_name)
            || transient::transient_leaf_is_reserved(&state, parent, &park_name)
            || transient::transient_directory_identity_is_reserved(&state, identity)
        {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory park name is reserved by a transient destination",
            ));
        }
        if state.park_conflicts(&key) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory park ownership is already retained",
            ));
        }
        let id = state.next_directory_park_id;
        state.next_directory_park_id = state
            .next_directory_park_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("directory park registry identity overflowed"))?;
        state.reserve_effect()?;
        assert!(state
            .directory_parks
            .insert(
                id,
                DirectoryParkRegistryRecord {
                    parent: parent.clone(),
                    original_name,
                    name: park_name,
                    identity,
                    cleanup,
                    phase,
                },
            )
            .is_none());
        assert!(
            state
                .park_owners
                .insert(key, ParkRegistryOwner::Directory(id))
                .is_none()
        );
        Ok(DirectoryParkRegistryToken {
            id,
            authority: Arc::downgrade(self),
            armed: true,
        })
    }

    fn take_directory_park(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &DirectoryParkRegistryToken,
    ) -> io::Result<DirectoryParkRecordGuard> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let checked_out = state
            .directory_parks_checked_out
            .checked_add(1)
            .ok_or_else(|| io::Error::other("checked-out directory park count overflowed"))?;
        let key = state
            .directory_parks
            .get(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
            })?
            .key();
        if state.park_owners.get(&key) != Some(&ParkRegistryOwner::Directory(token.id)) {
            return Err(io::Error::other(
                "directory park ownership index is inconsistent",
            ));
        }
        let record = state
            .directory_parks
            .remove(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
            })?;
        state.directory_parks_checked_out = checked_out;
        drop(state);
        Ok(DirectoryParkRecordGuard {
            authority: self.clone(),
            id: token.id,
            record: Some(record),
        })
    }

    fn rollback_directory_park_registration(
        self: &Arc<Self>,
        operation: &CapabilityOperation,
        token: &mut DirectoryParkRegistryToken,
    ) -> io::Result<()> {
        if !token.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || token.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let key = state
            .directory_parks
            .get(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
            })?
            .key();
        if state.park_owners.get(&key) != Some(&ParkRegistryOwner::Directory(token.id)) {
            return Err(io::Error::other(
                "directory park ownership index is inconsistent",
            ));
        }
        let record = state
            .directory_parks
            .remove(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
            })?;
        let removed = state
            .park_owners
            .remove(&record.key())
            .expect("prevalidated directory park owner remains registered");
        assert_eq!(removed, ParkRegistryOwner::Directory(token.id));
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    fn abandon_file_park(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.file_parks.get_mut(&id) {
            record.phase = FileParkRegistryPhase::Abandoned;
        }
    }

    fn abandon_directory_park(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.directory_parks.get_mut(&id) {
            record.phase = DirectoryParkRegistryPhase::Abandoned;
        }
    }

    fn abandon_stage(&self, id: u64) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(record) = state.stages.get_mut(&id) {
            record.carrier = StageCarrierState::Abandoned;
        }
    }

    fn update_stage(&self, id: u64, phase: StageRegistryPhase) -> io::Result<()> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let record = state
            .stages
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent"))?;
        record.phase = phase;
        if matches!(phase, StageRegistryPhase::Writing | StageRegistryPhase::Sealed) {
            record.destination = None;
        }
        Ok(())
    }

    fn prepare_stage_promotion(
        &self,
        id: u64,
        destination: Directory,
        name: LeafName,
    ) -> io::Result<()> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
            && !(state.phase == AUTHORITY_QUIESCING
                && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        if transient::transient_leaf_is_reserved(&state, &destination, &name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "promotion destination is reserved by a transient effect",
            ));
        }
        let record = state
            .stages
            .get_mut(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent"))?;
        record.destination = Some(NamespaceLeaf {
            parent: destination,
            name,
        });
        record.phase = StageRegistryPhase::PromotionAttempted;
        Ok(())
    }

    fn disarm_stage(&self, id: u64, operation: &CapabilityOperation) -> io::Result<()> {
        assert!(Arc::ptr_eq(&operation.authority, self));
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        state
            .stages
            .remove(&id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent"))?;
        state.release_effect(operation);
        Ok(())
    }

    fn cleanup_stage(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
                && !(state.phase == AUTHORITY_QUIESCING
                    && terminal_effect_settlement_admits(self))
            {
                return Err(stale_capability());
            }
            let record = match state.stages.remove(&id) {
                Some(record) => record,
                None => return Ok(()),
            };
            state.active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                },
            )
        };
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| match record.phase {
                StageRegistryPhase::PromotionAttempted | StageRegistryPhase::Unresolved => {
                    let destination = record.destination.as_ref().ok_or_else(|| {
                        io::Error::other("promotion stage lost its destination authority")
                    })?;
                    destination.parent.validate(&operation)?;
                    let source = platform::file_binding_state(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        record.identity,
                    )?;
                    let published = platform::file_binding_state(
                        &destination.parent.inner.handle,
                        destination.name.as_os_str(),
                        record.identity,
                    )?;
                    match (source, published) {
                        (platform::BindingState::Exact, platform::BindingState::Absent) => {
                            platform::remove_parked_file(
                                &record.parent.inner.handle,
                                record.name.as_os_str(),
                                &mut record.cleanup,
                                record.identity,
                            )
                        }
                        (platform::BindingState::Absent, platform::BindingState::Exact) => {
                            sync_rename_parents(&record.parent, &destination.parent)?;
                            destination.parent.validate(&operation)
                        }
                        _ => Err(identity_changed(
                            "promotion-attempted stage topology is indeterminate",
                        )),
                    }
                }
                StageRegistryPhase::CleanupAttempted => {
                    if platform::settle_removed_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        &record.cleanup,
                        record.identity,
                    )
                    .is_ok()
                    {
                        Ok(())
                    } else {
                        platform::remove_parked_file(
                            &record.parent.inner.handle,
                            record.name.as_os_str(),
                            &mut record.cleanup,
                            record.identity,
                        )
                    }
                }
                StageRegistryPhase::Writing | StageRegistryPhase::Sealed => {
                    platform::remove_parked_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        &mut record.cleanup,
                        record.identity,
                    )
                }
            });
        if let Err(error) = result {
            record.phase = match record.phase {
                StageRegistryPhase::PromotionAttempted | StageRegistryPhase::Unresolved => {
                    StageRegistryPhase::Unresolved
                }
                _ => StageRegistryPhase::CleanupAttempted,
            };
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.stages.insert(id, record);
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.release_effect(&operation);
        }
        Ok(())
    }

    fn cleanup_abandoned_stage_create(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let record = state.stage_creations.remove(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
            })?;
            if !matches!(
                record.phase,
                StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
            ) {
                state.stage_creations.insert(id, record);
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "stage create authority is still live",
                ));
            }
            state.active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                },
            )
        };
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| {
                if record.cleanup.is_none() {
                    let created = record
                        .created
                        .as_ref()
                        .expect("abandoned stage create retains its created file");
                    let identity = platform::file_identity(created)?;
                    record.cleanup = Some(platform::clone_stage_cleanup(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        created,
                        identity,
                    )?);
                    record.identity = Some(identity);
                    drop(record.created.take());
                }
                let cleanup = record
                    .cleanup
                    .as_mut()
                    .expect("abandoned stage create retains cleanup authority");
                let identity = record
                    .identity
                    .ok_or_else(|| identity_changed("stage create identity is absent"))?;
                if record.phase == StageCreatePhase::CleanupAttempted
                    && platform::settle_removed_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                    .is_ok()
                {
                    Ok(())
                } else {
                    platform::remove_parked_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                }
            });
        if let Err(error) = result {
            record.phase = StageCreatePhase::CleanupAttempted;
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.stage_creations.insert(id, record);
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.release_effect(&operation);
        }
        Ok(())
    }

    fn cleanup_abandoned_directory_create(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            let record = state.directory_creations.remove(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
            if !matches!(
                record.phase,
                DirectoryCreateEffectPhase::Abandoned
                    | DirectoryCreateEffectPhase::CleanupAttempted
            ) {
                state.directory_creations.insert(id, record);
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "directory create authority is still live",
                ));
            }
            state.active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                },
            )
        };
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| {
                if record.cleanup.is_none() {
                    let created = record
                        .created
                        .as_ref()
                        .expect("abandoned directory create retains its created directory");
                    let identity = platform::directory_identity(created)?;
                    record.cleanup = Some(platform::open_parked_directory(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        identity,
                    )?);
                    record.identity = Some(identity);
                    drop(record.created.take());
                }
                let cleanup = record
                    .cleanup
                    .as_mut()
                    .expect("abandoned directory create retains cleanup authority");
                let identity = record
                    .identity
                    .ok_or_else(|| identity_changed("directory create identity is absent"))?;
                if record.phase == DirectoryCreateEffectPhase::CleanupAttempted
                    && platform::settle_removed_directory(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                    .is_ok()
                {
                    Ok(())
                } else {
                    platform::remove_parked_directory(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        cleanup,
                        identity,
                    )
                }
            });
        if let Err(error) = result {
            record.phase = DirectoryCreateEffectPhase::CleanupAttempted;
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.directory_creations.insert(id, record);
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.release_effect(&operation);
        }
        Ok(())
    }

    fn begin_terminal_drain(&self) -> io::Result<()> {
        {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_LIVE {
                return Err(stale_capability());
            }
            if state.active != 0
                || state.file_parks_checked_out != 0
                || state.directory_parks_checked_out != 0
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem session still has active capability operations",
                ));
            }
            state.phase = AUTHORITY_QUIESCING;
        }
        let mut quiescing = TerminalQuiescingRollback::new(self);

        let owners = {
            let state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_QUIESCING {
                return Err(stale_capability());
            }
            state
                .active_effect_owners
                .values()
                .cloned()
                .collect::<Vec<_>>()
        };
        for owner in &owners {
            owner.settle(true)?;
        }
        drop(owners);

        let disposal = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_QUIESCING {
                return Err(stale_capability());
            }
            if state.active != 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem effect settlement remained active during terminal drain",
                ));
            }
            if state
                .active_effect_owners
                .values()
                .any(|owner| owner.has_domain_pending())
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by a domain-sensitive effect",
                ));
            }
            if !state.moves.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by an unowned move effect",
                ));
            }
            state
                .effect_owner_handles
                .retain(|_, owner| owner.strong_count() > 0);
            let has_external_owner = state.effect_owner_handles.iter().any(|(id, owner)| {
                let authority_owned = usize::from(state.active_effect_owners.contains_key(id));
                owner.strong_count() > authority_owned
            });
            if has_external_owner {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "filesystem terminal drain is obstructed by a live effect owner",
                ));
            }
            let owners = state
                .active_effect_owners
                .values()
                .cloned()
                .collect::<Vec<_>>();
            let mut disposal = Vec::new();
            for owner in owners {
                disposal.extend(owner.take_for_terminal_disposal());
            }
            state.active_effect_owners.clear();
            disposal
        };
        drop(disposal);

        let mut state = self.operations.lock().map_err(|_| {
            io::Error::other("filesystem capability operation lock was poisoned")
        })?;
        if state.phase != AUTHORITY_QUIESCING {
            return Err(stale_capability());
        }
        if state.active != 0
            || state.file_parks_checked_out != 0
            || state.directory_parks_checked_out != 0
            || !state.active_effect_owners.is_empty()
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem effect cleanup raced with terminal drain",
            ));
        }
        validate_terminal_registry_state(&state)?;
        state.phase = AUTHORITY_DRAINING;
        quiescing.disarm();
        Ok(())
    }

    fn restore_live_after_quiescing(&self) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.phase == AUTHORITY_QUIESCING {
            state.phase = AUTHORITY_LIVE;
        }
    }

    fn try_finish_terminal_drain(
        self: &Arc<Self>,
        terminal_phase: u8,
        validate_owned_root: bool,
    ) -> io::Result<SessionDrainSettlement> {
        let (
            cleanup_ids,
            create_cleanup_ids,
            directory_create_cleanup_ids,
            transient_cleanup_ids,
        ) = {
            let state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            if state.active != 0
                || state.file_parks_checked_out != 0
                || state.directory_parks_checked_out != 0
                || !state.moves.is_empty()
            {
                return Ok(SessionDrainSettlement::Pending);
            }
            (
                state
                    .stages
                    .iter()
                    .filter_map(|(id, record)| {
                        (record.carrier == StageCarrierState::Abandoned).then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .stage_creations
                    .iter()
                    .filter_map(|(id, record)| {
                        matches!(
                            record.phase,
                            StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
                        )
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .directory_creations
                    .iter()
                    .filter_map(|(id, record)| {
                        matches!(
                            record.phase,
                            DirectoryCreateEffectPhase::Abandoned
                                | DirectoryCreateEffectPhase::CleanupAttempted
                        )
                        .then_some(*id)
                    })
                    .collect::<Vec<_>>(),
                state
                    .transients
                    .iter()
                    .filter_map(|(id, record)| {
                        (record.phase == transient::TransientEffectPhase::Abandoned)
                            .then_some(*id)
                    })
                    .collect::<Vec<_>>(),
            )
        };
        if validate_owned_root {
            platform::validate_lease(&self.lease)?;
            platform::validate_root(&self.root)?;
            self.validate_retained_process_image_outside_root()?;
        }
        for id in cleanup_ids {
            let _ = self.cleanup_stage(id);
        }
        for id in create_cleanup_ids {
            let _ = self.cleanup_abandoned_stage_create(id);
        }
        for id in directory_create_cleanup_ids {
            let _ = self.cleanup_abandoned_directory_create(id);
        }
        let mut transient_cleanup_blocked = false;
        for id in transient_cleanup_ids {
            if self.cleanup_abandoned_transient(id).is_err() {
                transient_cleanup_blocked = true;
            }
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.active != 0
            || state.file_parks_checked_out != 0
            || state.directory_parks_checked_out != 0
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || state.directory_creations.values().any(|record| {
                record.phase != DirectoryCreateEffectPhase::UnclassifiedAbandoned
                    && !(terminal_phase == AUTHORITY_RESETTING
                        && record.phase
                            == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending)
            })
            || transient_cleanup_blocked
            || !state.transients.is_empty()
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        let abandoned_count = state
            .file_parks
            .values()
            .filter(|record| record.phase == FileParkRegistryPhase::Abandoned)
            .count()
            .checked_add(
                state
                    .directory_parks
                    .values()
                    .filter(|record| record.phase == DirectoryParkRegistryPhase::Abandoned)
                    .count(),
            )
            .and_then(|count| {
                count.checked_add(
                    state
                        .directory_creations
                        .values()
                        .filter(|record| {
                            record.phase
                                == DirectoryCreateEffectPhase::UnclassifiedAbandoned
                        })
                        .count(),
                )
            })
            .ok_or_else(|| io::Error::other("abandoned effect recovery count overflowed"))?;
        debug_assert!(abandoned_count <= MAX_OUTSTANDING_EFFECTS);
        if abandoned_count != 0 {
            let mut files = Vec::new();
            let mut directories = Vec::new();
            let mut directory_create_preservations = Vec::new();
            let mut permits = Vec::with_capacity(abandoned_count);
            for (id, record) in &mut state.file_parks {
                if record.phase != FileParkRegistryPhase::Abandoned {
                    continue;
                }
                record.phase = FileParkRegistryPhase::Live;
                files.push(ParkedFile {
                    parent: record.parent.clone(),
                    original_name: record.original_name.clone(),
                    park_name: record.name.clone(),
                    identity: record.identity,
                    size: record.size,
                    stamp: record.stamp,
                    verified: record.expected_digest.is_none(),
                    token: FileParkRegistryToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                    authority: Arc::downgrade(self),
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::File(*id),
                });
            }
            for (id, record) in &mut state.directory_parks {
                if record.phase != DirectoryParkRegistryPhase::Abandoned {
                    continue;
                }
                record.phase = DirectoryParkRegistryPhase::Live;
                directories.push(ParkedDirectory {
                    parent: record.parent.clone(),
                    original_name: record.original_name.clone(),
                    park_name: record.name.clone(),
                    identity: DirectoryIdentity {
                        session: self.session_nonce,
                        physical: record.identity,
                    },
                    token: DirectoryParkRegistryToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                    authority: Arc::downgrade(self),
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::Directory(*id),
                });
            }
            for (id, record) in &mut state.directory_creations {
                if record.phase != DirectoryCreateEffectPhase::UnclassifiedAbandoned {
                    continue;
                }
                record.phase = DirectoryCreateEffectPhase::UnclassifiedRecovery;
                directory_create_preservations.push(DirectoryCreatePreservation {
                    token: DirectoryCreateEffectToken {
                        id: *id,
                        authority: Arc::downgrade(self),
                        armed: true,
                    },
                });
                permits.push(DrainRecoveryPermit {
                    authority: self.clone(),
                    park_token_id: DrainRecoveryParkId::DirectoryCreate(*id),
                });
            }
            return Ok(SessionDrainSettlement::Recovery {
                recovery: SessionDrainRecoveryState {
                    files,
                    directories,
                    directory_create_preservations,
                    file_removals: Vec::new(),
                    file_restores: Vec::new(),
                    directory_removals: Vec::new(),
                    directory_restores: Vec::new(),
                },
                permits,
            });
        }
        let reset_pending_count = state
            .directory_creations
            .values()
            .filter(|record| {
                record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
            })
            .count();
        let expected_outstanding = if terminal_phase == AUTHORITY_RESETTING {
            reset_pending_count
        } else {
            0
        };
        if terminal_phase != AUTHORITY_RESETTING && state.outstanding_effects != 0 {
            return Ok(SessionDrainSettlement::Pending);
        }
        if !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.park_owners.is_empty()
            || !state.transients.is_empty()
            || state.outstanding_effects != expected_outstanding
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        drop(state);
        if validate_owned_root {
            self.validate_retained_process_image_outside_root()?;
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_DRAINING {
            return Err(stale_capability());
        }
        let directory_creations_settled = state.directory_creations.is_empty()
            || (terminal_phase == AUTHORITY_RESETTING
                && state.directory_creations.len() == reset_pending_count
                && state.directory_creations.values().all(|record| {
                    record.phase
                        == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
                }));
        if state.active != 0
            || state.outstanding_effects != expected_outstanding
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !directory_creations_settled
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.park_owners.is_empty()
            || !state.transients.is_empty()
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        state.phase = terminal_phase;
        Ok(SessionDrainSettlement::Ready)
    }
}

struct OperationState {
    phase: u8,
    active: usize,
    outstanding_effects: usize,
    next_move_id: u64,
    moves: HashMap<u64, MoveEffectRecord>,
    next_effect_owner_id: u64,
    effect_owner_handles: HashMap<u64, Weak<EffectOwnerState>>,
    active_effect_owners: HashMap<u64, Arc<EffectOwnerState>>,
    next_stage_id: u64,
    stages: HashMap<u64, StageRecord>,
    next_stage_create_id: u64,
    stage_creations: HashMap<u64, StageCreateRecord>,
    next_directory_create_id: u64,
    directory_creations: HashMap<u64, DirectoryCreateEffectRecord>,
    next_file_park_id: u64,
    file_parks: HashMap<u64, FileParkRegistryRecord>,
    file_parks_checked_out: usize,
    next_directory_park_id: u64,
    directory_parks: HashMap<u64, DirectoryParkRegistryRecord>,
    directory_parks_checked_out: usize,
    park_owners: HashMap<ParkRegistryKey, ParkRegistryOwner>,
    next_transient_id: u64,
    transients: HashMap<u64, transient::TransientEffectRecord>,
}

impl OperationState {
    fn park_conflicts(&self, key: &ParkRegistryKey) -> bool {
        self.park_owners
            .keys()
            .any(|retained| retained.conflicts_with(key))
    }

    fn reserve_effect(&mut self) -> io::Result<()> {
        self.reserve_effects(1)
    }

    fn reserve_effects(&mut self, count: usize) -> io::Result<()> {
        let outstanding_effects = self
            .outstanding_effects
            .checked_add(count)
            .ok_or_else(|| io::Error::other("filesystem effect registry capacity overflowed"))?;
        if outstanding_effects > MAX_OUTSTANDING_EFFECTS {
            return Err(io::Error::other(
                "filesystem effect registry capacity is exhausted",
            ));
        }
        self.outstanding_effects = outstanding_effects;
        Ok(())
    }

    fn reserve_move_effect(&mut self, record: MoveEffectRecord) -> io::Result<u64> {
        let id = self.next_move_id;
        let next_id = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("filesystem move effect id overflowed"))?;
        self.reserve_effect()?;
        self.next_move_id = next_id;
        assert!(self.moves.insert(id, record).is_none());
        Ok(id)
    }

    fn release_effect(&mut self, _operation: &CapabilityOperation) {
        assert!(
            self.active > 0,
            "filesystem effect release requires a live operation"
        );
        assert!(
            self.outstanding_effects > 0,
            "filesystem effect registry count underflowed"
        );
        self.outstanding_effects -= 1;
    }

}

struct CapabilityOperation {
    authority: Arc<CapabilityAuthority>,
}

impl Drop for CapabilityOperation {
    fn drop(&mut self) {
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        assert!(state.active > 0, "filesystem capability operation count underflowed");
        state.active -= 1;
    }
}

impl StageToken {
    fn update(&self, phase: StageRegistryPhase) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .update_stage(self.id, phase)
    }

    fn prepare_promotion(&self, destination: &Directory, name: &LeafName) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .prepare_stage_promotion(self.id, destination.clone(), name.clone())
    }

    fn discard(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .cleanup_stage(self.id)?;
        self.armed = false;
        Ok(())
    }

    fn disarm(&mut self) -> io::Result<()> {
        if !self.armed {
            return Ok(());
        }
        let authority = self
            .authority
            .upgrade()
            .ok_or_else(stale_capability)?;
        let operation = authority.enter()?;
        authority.disarm_stage(self.id, &operation)?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for StageToken {
    fn drop(&mut self) {
        if self.armed && self.discard().is_err() {
            if let Some(authority) = self.authority.upgrade() {
                authority.abandon_stage(self.id);
            }
            self.armed = false;
        }
    }
}

impl Drop for StageCreateToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_stage_create(self.id);
        }
    }
}

impl Drop for DirectoryCreateEffectToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_directory_create(self.id);
        }
    }
}

impl Drop for FileParkRegistryToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_file_park(self.id);
        }
    }
}

impl Drop for DirectoryParkRegistryToken {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(authority) = self.authority.upgrade() {
            authority.abandon_directory_park(self.id);
        }
    }
}

struct DirectoryInner {
    handle: platform::DirectoryHandle,
    identity: DirectoryIdentity,
    authority: Weak<CapabilityAuthority>,
    parent: Option<DirectoryParent>,
    absolute_ancestry: Option<platform::AbsoluteDirectoryGuard>,
}

struct DirectoryParent {
    directory: Directory,
    name: OsString,
}

#[derive(Clone)]
pub struct Directory {
    inner: Arc<DirectoryInner>,
}

/// An exact absolute directory admission retained without exposing its path or capability.
pub struct AdmittedAbsoluteDirectory {
    inner: Arc<AdmittedAbsoluteDirectoryInner>,
}

struct AdmittedAbsoluteDirectoryInner {
    directory: Directory,
}

impl_redacted_debug!(AdmittedAbsoluteDirectory);

impl AdmittedAbsoluteDirectory {
    pub fn revalidate(&self) -> io::Result<()> {
        let authority = self.inner.directory.authority()?;
        let operation = authority.enter()?;
        self.inner.directory.validate(&operation)
    }

    pub fn filesystem_identity(&self) -> io::Result<DirectoryFilesystemIdentity> {
        self.revalidate()?;
        let identity = self.inner.directory.identity()?.filesystem_identity();
        self.revalidate()?;
        Ok(identity)
    }

    pub fn acquire_root_session(&self) -> io::Result<AdmittedRootSessionAcquireOutcome> {
        self.revalidate()?;
        let ancestry = self
            .inner
            .directory
            .inner
            .absolute_ancestry
            .as_ref()
            .ok_or_else(|| io::Error::other("absolute directory admission lost its ancestry"))?;
        Ok(match RootSession::acquire_absolute_directory_guard(ancestry) {
            RootSessionAcquireOutcome::Acquired(session) => {
                AdmittedRootSessionAcquireOutcome::Acquired(AdmittedRootSession {
                    admission: Arc::clone(&self.inner),
                    session,
                })
            }
            RootSessionAcquireOutcome::NoEffect(error) => {
                AdmittedRootSessionAcquireOutcome::NoEffect(error)
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                AdmittedRootSessionAcquireOutcome::AppliedUnverified(
                    AdmittedRootSessionAcquireObligation {
                        admission: Arc::clone(&self.inner),
                        obligation: Some(obligation),
                    },
                )
            }
        })
    }
}

impl fmt::Debug for Directory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Directory").finish_non_exhaustive()
    }
}

impl Directory {
    pub fn create_effect_owner(&self) -> io::Result<EffectOwner> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.create_effect_owner(self.clone(), &operation)
    }

    fn is_within(&self, anchor: &Directory) -> bool {
        if self.inner.authority.as_ptr() != anchor.inner.authority.as_ptr() {
            return false;
        }
        let mut current = self;
        loop {
            if current.inner.identity == anchor.inner.identity {
                return true;
            }
            let Some(parent) = current.inner.parent.as_ref() else {
                return false;
            };
            current = &parent.directory;
        }
    }

    pub fn move_no_replace(
        self,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> DirectoryMoveOutcome {
        let Some(binding) = self.inner.parent.as_ref() else {
            return DirectoryMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a root directory capability cannot be moved",
                ),
                directory: self,
            };
        };
        let source_parent = binding.directory.clone();
        let source_name = match LeafName::new(binding.name.clone()) {
            Ok(name) => name,
            Err(_) => {
                return DirectoryMoveOutcome::NoEffect {
                    error: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory binding is not a valid native leaf",
                    ),
                    directory: self,
                };
            }
        };
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| destination.validate(&operation))
        {
            return DirectoryMoveOutcome::NoEffect {
                error,
                directory: self,
            };
        }
        if !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority) {
            return DirectoryMoveOutcome::NoEffect {
                error: stale_capability(),
                directory: self,
            };
        }
        if source_parent.inner.identity == destination.inner.identity
            && platform::leaf_names_equal(
                source_name.as_os_str(),
                destination_name.as_os_str(),
            )
        {
            return DirectoryMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory move destination matches its source",
                ),
                directory: self,
            };
        }
        let mut token = match MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: source_parent.clone(),
                name: source_name.clone(),
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            Some(self.inner.identity.physical),
        ) {
            Ok(token) => token,
            Err(error) => {
                return DirectoryMoveOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        let effect = platform::rename_directory_no_replace(
            &source_parent.inner.handle,
            source_name.as_os_str(),
            &self.inner.handle,
            self.inner.identity.physical,
            &destination.inner.handle,
            destination_name.as_os_str(),
        );
        let reported_success = effect.is_ok();
        match settle_directory_move(
            self,
            destination,
            destination_name,
            reported_success,
            &mut token,
        ) {
            Ok((true, directory)) => DirectoryMoveOutcome::Applied(directory),
            Ok((false, directory)) => DirectoryMoveOutcome::NoEffect {
                error: effect.err().unwrap_or_else(|| {
                    io::Error::other("directory move reported no effect after native success")
                }),
                directory,
            },
            Err(directory) => DirectoryMoveOutcome::AppliedUnverified(
                DirectoryMoveObligation {
                    error: effect.err().unwrap_or_else(|| {
                        io::Error::other("directory move could not be classified")
                    }),
                    directory: Some(directory),
                    destination: destination.clone(),
                    destination_name: destination_name.clone(),
                    reported_success,
                    token,
                },
            ),
        }
    }

    pub fn park(self) -> DirectoryParkOutcome {
        let mut directory = self;
        let mut last_collision = None;
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let park_name = random_leaf(".axial-dir-park-");
            match directory.park_as(park_name) {
                DirectoryParkOutcome::NoEffect {
                    error,
                    directory: returned,
                } if error.kind() == io::ErrorKind::AlreadyExists => {
                    directory = returned;
                    last_collision = Some(error);
                }
                outcome => return outcome,
            }
        }
        DirectoryParkOutcome::NoEffect {
            error: last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique parked directory name",
                )
            }),
            directory,
        }
    }

    pub fn park_as(self, park_name: LeafName) -> DirectoryParkOutcome {
        let Some(binding) = self.inner.parent.as_ref() else {
            return DirectoryParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "a root directory capability cannot be parked",
                ),
                directory: self,
            };
        };
        let parent = binding.directory.clone();
        let original_name = match LeafName::new(binding.name.clone()) {
            Ok(name) => name,
            Err(_) => {
                return DirectoryParkOutcome::NoEffect {
                    error: io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "directory binding is not a valid native leaf",
                    ),
                    directory: self,
                };
            }
        };
        if platform::leaf_names_equal(original_name.as_os_str(), park_name.as_os_str()) {
            return DirectoryParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory park destination matches its source",
                ),
                directory: self,
            };
        }
        let authority = match parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryParkOutcome::NoEffect {
                error,
                directory: self,
            };
        }
        let key = ParkRegistryKey::new(
            &parent,
            &original_name,
            &park_name,
            self.inner.identity.physical,
        );
        if let Err(error) = authority.ensure_park_available(&operation, &key) {
            return DirectoryParkOutcome::NoEffect {
                error,
                directory: self,
            };
        }
        let cleanup = match platform::open_parked_directory(
            &parent.inner.handle,
            original_name.as_os_str(),
            self.inner.identity.physical,
        ) {
            Ok(cleanup) => cleanup,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        let mut token = match authority.reserve_directory_park(
            &operation,
            &parent,
            &self,
            original_name.clone(),
            park_name.clone(),
            cleanup,
        ) {
            Ok(token) => token,
            Err(error) => {
                return DirectoryParkOutcome::NoEffect {
                    error,
                    directory: self,
                };
            }
        };
        let mut guard = match authority.take_directory_park(&operation, &token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                    error,
                    parent,
                    directory: Some(self),
                    original_name,
                    token,
                    park_name,
                });
            }
        };
        let effect = platform::park_directory_no_replace(
            &parent.inner.handle,
            original_name.as_os_str(),
            &self.inner.handle,
            self.inner.identity.physical,
            park_name.as_os_str(),
            &guard.record().cleanup,
        );
        match effect {
            Ok(()) => {
                if let Err(error) = parent.validate(&operation) {
                    drop(guard);
                    return DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                        error,
                        parent,
                        directory: Some(self),
                        original_name,
                        token,
                        park_name,
                    });
                }
                guard.record_mut().phase = DirectoryParkRegistryPhase::Live;
                drop(guard);
                DirectoryParkOutcome::Parked(ParkedDirectory {
                    parent,
                    original_name,
                    park_name,
                    identity: self.inner.identity,
                    token,
                    authority: self.inner.authority.clone(),
                })
            }
            Err(platform::ParkDirectoryError::NoEffect(error)) => {
                guard.disarm(&mut token, &operation);
                DirectoryParkOutcome::NoEffect {
                    error,
                    directory: self,
                }
            }
            Err(platform::ParkDirectoryError::AppliedUnverified(error)) => {
                drop(guard);
                DirectoryParkOutcome::AppliedUnverified(DirectoryParkObligation {
                    error,
                    parent,
                    directory: Some(self),
                    original_name,
                    token,
                    park_name,
                })
            }
        }
    }

    fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        let mut current = self.inner.as_ref();
        loop {
            if current.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
                return Err(stale_capability());
            }
            if platform::directory_identity(&current.handle)? != current.identity.physical {
                return Err(identity_changed("directory capability changed identity"));
            }
            if let Some(ancestry) = &current.absolute_ancestry {
                platform::validate_absolute_directory_guard(ancestry)?;
            }
            let Some(binding) = &current.parent else {
                break;
            };
            if platform::directory_binding_state(
                &binding.directory.inner.handle,
                &binding.name,
                current.identity.physical,
            )? != platform::BindingState::Exact
            {
                return Err(identity_changed("directory capability changed binding"));
            }
            current = binding.directory.inner.as_ref();
        }
        Ok(())
    }
}

fn settle_directory_park(
    mut obligation: DirectoryParkObligation,
    force_restore: bool,
) -> DirectoryParkResolution {
    let directory_identity = obligation
        .directory
        .as_ref()
        .expect("directory park obligation retains directory")
        .inner
        .identity;
    let authority = match obligation.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    if obligation.parent.validate(&operation).is_err() {
        return DirectoryParkResolution::Indeterminate(obligation);
    }
    let mut guard = match authority.take_directory_park(&operation, &obligation.token) {
        Ok(guard) => guard,
        Err(_) => return DirectoryParkResolution::Indeterminate(obligation),
    };
    let original = platform::directory_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().original_name.as_os_str(),
        guard.record().identity,
    );
    let parked_state = platform::directory_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        guard.record().identity,
    );
    match (original, parked_state) {
        (Ok(platform::BindingState::Exact), Ok(platform::BindingState::Absent)) => {
            guard.disarm(&mut obligation.token, &operation);
            DirectoryParkResolution::NoEffect(
                obligation.directory.take().expect("parked directory"),
            )
        }
        (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
            if force_restore {
                let restoration = {
                    let record = guard.record_mut();
                    platform::restore_parked_directory(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        &mut record.cleanup,
                        record.identity,
                        record.original_name.as_os_str(),
                    )
                };
                return match restoration {
                    Ok(_)
                        if obligation
                            .directory
                            .as_ref()
                            .expect("directory park obligation retains directory")
                            .validate(&operation)
                            .is_ok() =>
                    {
                        guard.disarm(&mut obligation.token, &operation);
                        DirectoryParkResolution::NoEffect(
                            obligation.directory.take().expect("parked directory"),
                        )
                    }
                    _ => DirectoryParkResolution::Indeterminate(obligation),
                };
            }
            let directory = obligation.directory.take().expect("parked directory");
            if obligation.parent.validate(&operation).is_err() {
                return DirectoryParkResolution::Indeterminate(obligation);
            }
            guard.record_mut().phase = DirectoryParkRegistryPhase::Live;
            drop(guard);
            DirectoryParkResolution::Parked(ParkedDirectory {
                parent: obligation.parent,
                original_name: obligation.original_name,
                park_name: obligation.park_name,
                identity: directory_identity,
                token: obligation.token,
                authority: directory.inner.authority.clone(),
            })
        }
        _ => DirectoryParkResolution::Indeterminate(obligation),
    }
}

impl ParkedDirectory {
    pub fn remove_empty(mut self) -> DirectoryRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryRemovalOutcome::Removed
            }
            Ok(()) => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                error: identity_changed("directory removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    DirectoryRemovalOutcome::NoEffect { error, parked: self }
                }
                _ => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn remove_empty_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        self.remove_empty_admitted(authority, operation)
    }

    fn remove_empty_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRemovalOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return DirectoryRemovalOutcome::NoEffect { error, parked: self },
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryRemovalOutcome::Removed
            }
            Ok(()) => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                error: identity_changed("directory removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    DirectoryRemovalOutcome::NoEffect { error, parked: self }
                }
                _ => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    /// Removes every descendant bound inside the claimed parked root without
    /// following links or reparse points. Entries concurrently introduced
    /// inside that deletion root are part of the deletion scope; its original
    /// and parked sibling bindings remain outside that scope.
    ///
    /// The capability authority and root lease serialize cooperating namespace
    /// writers. A non-cooperating process that concurrently rewrites the same
    /// private namespace is outside that authority; Linux has no unprivileged
    /// handle-targeted unlink that could close its final name/unlink race.
    pub fn remove_tree(self) -> DirectoryTreeRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        self.remove_tree_admitted(authority, operation)
    }

    fn remove_tree_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryTreeRemovalOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryTreeRemovalOutcome::Indeterminate(
                DirectoryTreeRemovalObligation {
                    error,
                    parked: Some(self),
                },
            );
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryTreeRemovalOutcome::Retained {
                    error,
                    retained: RetainedDirectoryTreeRemoval::new(self),
                };
            }
        };
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory_tree(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if self.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut self.token, &operation);
                DirectoryTreeRemovalOutcome::Removed
            }
            Ok(()) => DirectoryTreeRemovalOutcome::Indeterminate(
                DirectoryTreeRemovalObligation {
                    error: identity_changed("directory tree removal lost its authority chain"),
                    parked: Some(self),
                },
            ),
            Err(error) => DirectoryTreeRemovalOutcome::Indeterminate(
                DirectoryTreeRemovalObligation {
                    error,
                    parked: Some(self),
                },
            ),
        }
    }

    pub fn restore(mut self) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_directory(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    self.identity,
                    self.authority.clone(),
                    Some(DirectoryParent {
                        directory: self.parent.clone(),
                        name: self.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    DirectoryRestoreOutcome::Restored(restored)
                } else {
                    DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error: identity_changed("restored directory lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    DirectoryRestoreOutcome::NoEffect { error, parked: self }
                }
                _ => DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn restore_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        self.restore_admitted(authority, operation)
    }

    fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect { error, parked: self };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => return DirectoryRestoreOutcome::NoEffect { error, parked: self },
        };
        let restoration = {
            let record = guard.record_mut();
            platform::restore_parked_directory(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
        };
        match restoration {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    self.identity,
                    self.authority.clone(),
                    Some(DirectoryParent {
                        directory: self.parent.clone(),
                        name: self.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut self.token, &operation);
                    DirectoryRestoreOutcome::Restored(restored)
                } else {
                    DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error: identity_changed("restored directory lost its authority chain"),
                        parked: Some(self),
                    })
                }
            }
            Err(error) => {
                drop(guard);
                match self.binding_state() {
                Ok(platform::BindingState::Exact) if self.validate(&operation).is_ok() => {
                    DirectoryRestoreOutcome::NoEffect { error, parked: self }
                }
                _ => DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                    error,
                    parked: Some(self),
                }),
                }
            }
        }
    }

    fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.authority.upgrade().ok_or_else(stale_capability)
    }

    fn binding_state(&self) -> io::Result<platform::BindingState> {
        platform::directory_binding_state(
            &self.parent.inner.handle,
            self.park_name.as_os_str(),
            self.identity.physical,
        )
    }

    fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.token.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        {
            return Err(identity_changed("parked directory capability changed"));
        }
        let registered = {
            let state = operation.authority.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            state
                .directory_parks
                .get(&self.token.id)
                .is_some_and(|record| {
                    record.phase == DirectoryParkRegistryPhase::Live
                        && record.identity == self.identity.physical
                        && record.name == self.park_name
                        && record.original_name == self.original_name
                })
        };
        if !registered || self.binding_state()? != platform::BindingState::Exact {
            return Err(identity_changed("parked directory capability changed"));
        }
        Ok(())
    }
}

impl DirectoryRemovalObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> DirectoryRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_admitted(
        mut self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_removed_directory(
            &guard.record().parent.inner.handle,
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        ) {
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                DirectoryRemovalResolution::Removed
            }
            Ok(()) => {
                drop(operation);
                self.parked = Some(parked);
                DirectoryRemovalResolution::Indeterminate(self)
            }
            Err(_) => {
                drop(operation);
                self.parked = Some(parked);
                DirectoryRemovalResolution::Indeterminate(self)
            }
        }
    }

    fn retain(mut self, parked: ParkedDirectory) -> DirectoryRemovalResolution {
        self.parked = Some(parked);
        DirectoryRemovalResolution::Indeterminate(self)
    }
}

impl DirectoryTreeRemovalObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> DirectoryTreeRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("tree removal obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_admitted(
        mut self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryTreeRemovalResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let mut guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        let settled = platform::settle_removed_directory(
            &guard.record().parent.inner.handle,
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
        );
        if settled.is_ok() && parked.parent.validate(&operation).is_ok() {
            guard.disarm(&mut parked.token, &operation);
            return DirectoryTreeRemovalResolution::Removed;
        }
        if settled.is_ok() {
            drop(guard);
            drop(operation);
            return self.retain(parked);
        }
        let removal = {
            let record = guard.record_mut();
            platform::remove_parked_directory_tree(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
            )
        };
        match removal {
            Ok(()) if parked.parent.validate(&operation).is_ok() => {
                guard.disarm(&mut parked.token, &operation);
                DirectoryTreeRemovalResolution::Removed
            }
            _ => {
                drop(guard);
                drop(operation);
                self.retain(parked)
            }
        }
    }

    fn retain(mut self, parked: ParkedDirectory) -> DirectoryTreeRemovalResolution {
        self.parked = Some(parked);
        DirectoryTreeRemovalResolution::Indeterminate(self)
    }
}

impl DirectoryRestoreObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> DirectoryRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park(&parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_with_recovery(
        mut self,
        permit: &DrainRecoveryPermit,
    ) -> DirectoryRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked directory");
        let authority = match parked.authority() {
            Ok(authority) => authority,
            Err(_) => return self.retain(parked),
        };
        let operation = match authority.enter_directory_park_recovery(permit, &parked.token) {
            Ok(operation) => operation,
            Err(_) => return self.retain(parked),
        };
        self.reconcile_admitted(parked, authority, operation)
    }

    fn reconcile_admitted(
        mut self,
        mut parked: ParkedDirectory,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRestoreResolution {
        if parked.parent.validate(&operation).is_err() {
            return self.retain(parked);
        }
        let guard = match authority.take_directory_park(&operation, &parked.token) {
            Ok(guard) => guard,
            Err(_) => return self.retain(parked),
        };
        match platform::settle_restored_directory(
            &guard.record().parent.inner.handle,
            guard.record().name.as_os_str(),
            &guard.record().cleanup,
            guard.record().identity,
            guard.record().original_name.as_os_str(),
        ) {
            Ok(handle) => {
                let restored = Directory::from_handle(
                    handle,
                    parked.identity,
                    parked.authority.clone(),
                    Some(DirectoryParent {
                        directory: parked.parent.clone(),
                        name: parked.original_name.as_os_str().to_os_string(),
                    }),
                );
                if restored.validate(&operation).is_ok() {
                    guard.disarm(&mut parked.token, &operation);
                    DirectoryRestoreResolution::Restored(restored)
                } else {
                    drop(guard);
                    drop(operation);
                    self.parked = Some(parked);
                    DirectoryRestoreResolution::Indeterminate(self)
                }
            }
            Err(_) => {
                drop(guard);
                match parked.binding_state() {
                Ok(platform::BindingState::Exact) if parked.validate(&operation).is_ok() => {
                    DirectoryRestoreResolution::NoEffect(parked)
                }
                _ => {
                    drop(operation);
                    self.parked = Some(parked);
                    DirectoryRestoreResolution::Indeterminate(self)
                }
                }
            }
        }
    }

    fn retain(mut self, parked: ParkedDirectory) -> DirectoryRestoreResolution {
        self.parked = Some(parked);
        DirectoryRestoreResolution::Indeterminate(self)
    }
}

impl Directory {
    fn is_managed_root_descendant(&self) -> bool {
        let mut current = self;
        loop {
            if current.inner.absolute_ancestry.is_some() {
                return false;
            }
            match current.inner.parent.as_ref() {
                Some(parent) => current = &parent.directory,
                None => return true,
            }
        }
    }

    pub fn revision(&self) -> io::Result<DirectoryRevision> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let stamp = platform::directory_revision(&self.inner.handle)?;
        self.validate(&operation)?;
        Ok(DirectoryRevision {
            identity: self.inner.identity,
            stamp,
        })
    }

    pub fn validate_revision(&self, expected: &DirectoryRevision) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)
    }

    fn validate_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &DirectoryRevision,
    ) -> io::Result<()> {
        if self.inner.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let stamp = platform::directory_revision(&self.inner.handle)?;
        if expected.identity != self.inner.identity || expected.stamp != stamp {
            return Err(identity_changed("directory revision changed"));
        }
        Ok(())
    }

    pub fn identity(&self) -> io::Result<DirectoryIdentity> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        Ok(self.inner.identity)
    }

    pub fn open_directory(&self, name: &LeafName) -> io::Result<Self> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_preserved_unclassified(&operation, self, name)?;
        let (handle, identity) = platform::open_directory(&self.inner.handle, name.as_os_str())?;
        let opened = Self::from_handle(
            handle,
            authority.identity(identity),
            self.inner.authority.clone(),
            Some(DirectoryParent {
                directory: self.clone(),
                name: name.as_os_str().to_os_string(),
            }),
        );
        opened.validate(&operation)?;
        Ok(opened)
    }

    pub fn create_directory(&self, name: &LeafName) -> DirectoryCreateOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryCreateOutcome::NoEffect(error);
        }
        if let Err(error) =
            authority.ensure_leaf_not_preserved_unclassified(&operation, self, name)
        {
            return DirectoryCreateOutcome::NoEffect(error);
        }
        let mut reservation = match authority.reserve_directory_create(&operation, self, name) {
            Ok(reservation) => reservation,
            Err(error) => return DirectoryCreateOutcome::NoEffect(error),
        };
        let handle = match platform::create_directory(&self.inner.handle, name.as_os_str()) {
            Ok(handle) => handle,
            Err(platform::CreateDirectoryError::NoEffect(error)) => {
                match authority.take_directory_create(&operation, &reservation) {
                    Ok(guard) => guard.disarm(&mut reservation, &operation),
                    Err(settlement) => {
                        return DirectoryCreateOutcome::AppliedUnverified(
                            DirectoryCreateObligation {
                                error: io::Error::other(format!(
                                    "directory create had no native effect but its reservation did not settle: {error}; {settlement}"
                                )),
                                token: reservation,
                            },
                        );
                    }
                }
                return DirectoryCreateOutcome::NoEffect(error);
            }
            Err(platform::CreateDirectoryError::CreatedUnclassified(error)) => {
                authority.mark_directory_create_unclassified(&reservation);
                return DirectoryCreateOutcome::CreatedUnclassified {
                    error,
                    preservation: DirectoryCreatePreservation { token: reservation },
                };
            }
            Err(platform::CreateDirectoryError::AppliedUnverified { error, retained }) => {
                authority.attach_directory_create(&reservation, retained);
                return DirectoryCreateOutcome::AppliedUnverified(DirectoryCreateObligation {
                    error,
                    token: reservation,
                });
            }
        };
        authority.attach_directory_create(&reservation, handle);
        match finish_directory_create(&authority, &operation, &mut reservation) {
            Ok(directory) => DirectoryCreateOutcome::Created(directory),
            Err(error) => {
                DirectoryCreateOutcome::AppliedUnverified(DirectoryCreateObligation {
                    error,
                    token: reservation,
                })
            }
        }
    }

    pub fn open_file(&self, name: &LeafName) -> io::Result<FileCapability> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_preserved_unclassified(&operation, self, name)?;
        authority.ensure_leaf_not_transient_reserved(&operation, self, name)?;
        let handle = platform::open_file(&self.inner.handle, name.as_os_str())?;
        let identity = platform::file_identity(&handle)?;
        let file = FileCapability::new(
            handle,
            identity,
            self.clone(),
            name.clone(),
            self.inner.authority.clone(),
        );
        file.validate(&operation)?;
        Ok(file)
    }

    pub fn create_file_create_only(&self, name: &LeafName) -> FileCreateOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        if let Err(error) = self.validate(&operation) {
            return FileCreateOutcome::NoEffect(error);
        }
        if let Err(error) =
            authority.ensure_leaf_not_preserved_unclassified(&operation, self, name)
        {
            return FileCreateOutcome::NoEffect(error);
        }
        let mut reservation = match authority.reserve_stage_create(&operation, self, name) {
            Ok(reservation) => reservation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        let handle = match platform::create_file(&self.inner.handle, name.as_os_str()) {
            Ok(handle) => handle,
            Err(platform::CreateFileError::NoEffect(error)) => {
                match authority.take_stage_create(&operation, &reservation) {
                    Ok(guard) => guard.disarm(&mut reservation, &operation),
                    Err(settlement) => {
                        return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                            error: io::Error::other(format!(
                                "stage create had no native effect but its reservation did not settle: {error}; {settlement}"
                            )),
                            token: reservation,
                        });
                    }
                }
                return FileCreateOutcome::NoEffect(error);
            }
            Err(platform::CreateFileError::AppliedUnverified { error, retained }) => {
                authority.attach_stage_create(&reservation, retained);
                return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                    error,
                    token: reservation,
                });
            }
        };
        authority.attach_stage_create(&reservation, handle);
        match finish_stage_create(&authority, &operation, &mut reservation) {
            Ok(staged) => FileCreateOutcome::Created(staged),
            Err(error) => FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                error,
                token: reservation,
            }),
        }
    }

    pub fn create_stage(&self) -> FileCreateOutcome {
        use rand::RngCore;

        for _ in 0..MAX_STAGE_ATTEMPTS {
            let mut nonce = [0_u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut nonce);
            let name = format!(".axial-stage-{}", encode_hex(&nonce));
            let name = LeafName::new(name).expect("generated stage leaf is valid");
            match self.create_file_create_only(&name) {
                FileCreateOutcome::NoEffect(error)
                    if error.kind() == io::ErrorKind::AlreadyExists => {}
                outcome => return outcome,
            }
        }
        FileCreateOutcome::NoEffect(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique staged file",
        ))
    }

    pub fn entries(&self, limit: usize) -> io::Result<DirectoryListing> {
        if limit == 0 || limit > MAX_DIRECTORY_LIST_ENTRIES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory listing limit is outside the supported range",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let listing = platform::entries(&self.inner.handle, limit)?;
        self.validate(&operation)?;
        let state = if listing.complete {
            DirectoryListingState::Complete
        } else {
            DirectoryListingState::Truncated
        };
        let entries = listing
            .entries
            .into_iter()
            .map(|(name, kind)| DirectoryEntry {
                name,
                kind,
                parent: self.inner.identity,
            })
            .collect();
        Ok(DirectoryListing { entries, state })
    }

    pub fn open_observed_directory(&self, entry: &DirectoryEntry) -> io::Result<Self> {
        if entry.parent != self.inner.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "observed entry belongs to another directory",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let name = LeafName::new(entry.name.clone()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "observed entry name is invalid")
        })?;
        authority.ensure_leaf_not_preserved_unclassified(&operation, self, &name)?;
        let (handle, identity) =
            platform::open_directory(&self.inner.handle, entry.name.as_os_str())?;
        let opened = Self::from_handle(
            handle,
            authority.identity(identity),
            self.inner.authority.clone(),
            Some(DirectoryParent {
                directory: self.clone(),
                name: entry.name.clone(),
            }),
        );
        opened.validate(&operation)?;
        Ok(opened)
    }

    pub fn admit_existing_file_park(
        &self,
        original_name: &LeafName,
        parked: FileParkRequest,
    ) -> io::Result<ParkedFile> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        parked.file.validate_bound_to(self, &operation)?;
        if platform::leaf_names_equal(original_name.as_os_str(), parked.file.name.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file park original and parked leaves must differ",
            ));
        }
        parked.validate_revision(&operation)?;
        if platform::file_binding_state(
            &self.inner.handle,
            original_name.as_os_str(),
            parked.file.identity,
        )? != platform::BindingState::Absent
            || platform::file_binding_state(
                &self.inner.handle,
                parked.file.name.as_os_str(),
                parked.file.identity,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed(
                "existing file park topology is not exact",
            ));
        }
        let key = ParkRegistryKey::new(
            self,
            original_name,
            &parked.file.name,
            parked.file.identity,
        );
        authority.ensure_park_available(&operation, &key)?;
        let cleanup = platform::open_parked_file(
            &self.inner.handle,
            parked.file.name.as_os_str(),
            parked.file.identity,
        )?;
        verify_parked_file(self, &parked.file.name, &cleanup, &parked.expected)?;
        self.validate(&operation)?;

        let park_name = parked.file.name.clone();
        let identity = parked.file.identity;
        let size = parked.expected.revision.size;
        let stamp = parked.expected.revision.stamp;
        let parked_authority = parked.file.authority.clone();
        let mut token = authority.register_file_park(
            &operation,
            self,
            original_name.clone(),
            park_name.clone(),
            identity,
            size,
            stamp,
            None,
            cleanup,
            FileParkRegistryPhase::Live,
        )?;

        let post_registration = (|| {
            self.validate(&operation)?;
            parked.validate_revision(&operation)?;
            if platform::file_binding_state(
                &self.inner.handle,
                original_name.as_os_str(),
                identity,
            )? != platform::BindingState::Absent
                || platform::file_binding_state(
                    &self.inner.handle,
                    park_name.as_os_str(),
                    identity,
                )? != platform::BindingState::Exact
            {
                return Err(identity_changed(
                    "existing file park topology changed after registration",
                ));
            }
            let guard = authority.take_file_park(&operation, &token)?;
            let proof = verify_parked_file(self, &park_name, &guard.record().cleanup, &parked.expected);
            drop(guard);
            proof?;
            parked.validate_revision(&operation)?;
            self.validate(&operation)
        })();
        if let Err(error) = post_registration {
            authority.rollback_file_park_registration(&operation, &mut token)?;
            return Err(error);
        }

        Ok(ParkedFile {
            parent: self.clone(),
            original_name: original_name.clone(),
            park_name,
            identity,
            size,
            stamp,
            verified: true,
            token,
            authority: parked_authority,
        })
    }

    pub fn admit_existing_directory_park(
        &self,
        original_name: &LeafName,
        parked: Directory,
        expected: &DirectoryRevision,
    ) -> io::Result<ParkedDirectory> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        parked.validate(&operation)?;
        let binding = parked.inner.parent.as_ref().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "a root directory cannot be admitted as a park",
            )
        })?;
        binding.directory.validate(&operation)?;
        if binding.directory.inner.identity != self.inner.identity {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "parked directory belongs to another parent authority",
            ));
        }
        let park_name = LeafName::new(binding.name.clone()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "parked directory binding is not a valid leaf",
            )
        })?;
        if platform::leaf_names_equal(original_name.as_os_str(), park_name.as_os_str()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory park original and parked leaves must differ",
            ));
        }
        parked.validate_revision_in(&operation, expected)?;
        if platform::directory_binding_state(
            &self.inner.handle,
            original_name.as_os_str(),
            parked.inner.identity.physical,
        )? != platform::BindingState::Absent
            || platform::directory_binding_state(
                &self.inner.handle,
                park_name.as_os_str(),
                parked.inner.identity.physical,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed(
                "existing directory park topology is not exact",
            ));
        }
        let key = ParkRegistryKey::new(
            self,
            original_name,
            &park_name,
            parked.inner.identity.physical,
        );
        authority.ensure_park_available(&operation, &key)?;
        let cleanup = platform::open_parked_directory(
            &self.inner.handle,
            park_name.as_os_str(),
            parked.inner.identity.physical,
        )?;
        parked.validate_revision_in(&operation, expected)?;
        self.validate(&operation)?;

        let identity = parked.inner.identity;
        let parked_authority = parked.inner.authority.clone();
        let mut token = authority.register_directory_park(
            &operation,
            self,
            original_name.clone(),
            park_name.clone(),
            identity.physical,
            cleanup,
            DirectoryParkRegistryPhase::Live,
        )?;

        let post_registration = (|| {
            self.validate(&operation)?;
            if platform::directory_binding_state(
                &self.inner.handle,
                original_name.as_os_str(),
                identity.physical,
            )? != platform::BindingState::Absent
                || platform::directory_binding_state(
                    &self.inner.handle,
                    park_name.as_os_str(),
                    identity.physical,
                )? != platform::BindingState::Exact
            {
                return Err(identity_changed(
                    "existing directory park topology changed after registration",
                ));
            }
            parked.validate_revision_in(&operation, expected)?;
            parked.validate(&operation)?;
            self.validate(&operation)
        })();
        if let Err(error) = post_registration {
            authority.rollback_directory_park_registration(&operation, &mut token)?;
            return Err(error);
        }

        Ok(ParkedDirectory {
            parent: self.clone(),
            original_name: original_name.clone(),
            park_name,
            identity,
            token,
            authority: parked_authority,
        })
    }

    pub fn park_file(&self, request: FileParkRequest) -> FileParkOutcome {
        let mut request = request;
        let mut last_collision = None;
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let park_name = random_leaf(".axial-park-");
            match self.park_file_as(request, park_name) {
                FileParkOutcome::NoEffect {
                    error,
                    request: returned,
                } if error.kind() == io::ErrorKind::AlreadyExists => {
                    request = returned;
                    last_collision = Some(error);
                }
                outcome => return outcome,
            }
        }
        FileParkOutcome::NoEffect {
            error: last_collision.unwrap_or_else(|| {
                io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    "could not reserve a unique parked file name",
                )
            }),
            request,
        }
    }

    pub fn park_file_as(
        &self,
        request: FileParkRequest,
        park_name: LeafName,
    ) -> FileParkOutcome {
        if platform::leaf_names_equal(request.file.name.as_os_str(), park_name.as_os_str()) {
            return FileParkOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file park destination matches its source",
                ),
                request,
            };
        }
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        if let Err(error) = self.validate(&operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        if let Err(error) = request.file.validate_bound_to(self, &operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        if let Err(error) = request.validate_revision(&operation) {
            return FileParkOutcome::NoEffect { error, request };
        }
        let key = ParkRegistryKey::new(
            self,
            &request.file.name,
            &park_name,
            request.file.identity,
        );
        if let Err(error) = authority.ensure_park_available(&operation, &key) {
            return FileParkOutcome::NoEffect { error, request };
        }
        let cleanup = match platform::open_parked_file(
            &self.inner.handle,
            request.file.name.as_os_str(),
            request.file.identity,
        ) {
            Ok(cleanup) => cleanup,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        let mut token = match authority.reserve_file_park(
            &operation,
            &request,
            park_name.clone(),
            cleanup,
        ) {
            Ok(token) => token,
            Err(error) => return FileParkOutcome::NoEffect { error, request },
        };
        let guard = match authority.take_file_park(&operation, &token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error,
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::Parking,
                    digest_verified: false,
                });
            }
        };
        let effect = platform::park_file_no_replace(
            &self.inner.handle,
            request.file.name.as_os_str(),
            &request.file.handle,
            request.file.identity,
            park_name.as_os_str(),
            &guard.record().cleanup,
        );
        match effect {
            Ok(()) => {
                drop(guard);
                finish_new_file_park(request, park_name, token, &operation)
            }
            Err(platform::ParkFileError::NoEffect(error)) => {
                guard.disarm(&mut token, &operation);
                FileParkOutcome::NoEffect { error, request }
            }
            Err(platform::ParkFileError::AppliedUnverified(error)) => {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error,
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::Parking,
                    digest_verified: false,
                })
            }
        }
    }

    pub fn sync(&self) -> io::Result<()> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        platform::sync_directory(&self.inner.handle)?;
        self.validate(&operation)
    }

    fn from_handle(
        handle: platform::DirectoryHandle,
        identity: DirectoryIdentity,
        authority: Weak<CapabilityAuthority>,
        parent: Option<DirectoryParent>,
    ) -> Self {
        Self {
            inner: Arc::new(DirectoryInner {
                handle,
                identity,
                authority,
                parent,
                absolute_ancestry: None,
            }),
        }
    }

    fn from_absolute_handle(
        handle: platform::DirectoryHandle,
        identity: DirectoryIdentity,
        authority: Weak<CapabilityAuthority>,
        absolute_ancestry: platform::AbsoluteDirectoryGuard,
    ) -> Self {
        Self {
            inner: Arc::new(DirectoryInner {
                handle,
                identity,
                authority,
                parent: None,
                absolute_ancestry: Some(absolute_ancestry),
            }),
        }
    }

    fn file_binding_matches(
        &self,
        file: &FileCapability,
        operation: &CapabilityOperation,
    ) -> io::Result<bool> {
        self.validate(operation)?;
        Ok(platform::file_binding_state(
            &self.inner.handle,
            file.name.as_os_str(),
            file.identity,
        )? == platform::BindingState::Exact)
    }

    fn authority(&self) -> io::Result<Arc<CapabilityAuthority>> {
        self.inner.authority.upgrade().ok_or_else(stale_capability)
    }
}

pub struct FileCapability {
    handle: File,
    identity: platform::Identity,
    parent: Directory,
    name: LeafName,
    authority: Weak<CapabilityAuthority>,
}

pub struct FileRevision {
    authority: Weak<CapabilityAuthority>,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
}

#[derive(Clone)]
pub struct FileRevisionObservation {
    authority: Weak<CapabilityAuthority>,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
}

#[must_use = "file revision readers must be explicitly finished or cancelled"]
pub struct FileRevisionReader {
    state: Option<FileRevisionReaderState>,
}

struct FileRevisionReaderState {
    file: FileCapability,
    expected: FileRevision,
    position: u64,
    operation: CapabilityOperation,
}

impl_redacted_debug!(FileRevisionReader);

#[must_use = "file revision reader start failures retain the file and revision and must be retried or unpacked"]
pub struct FileRevisionReaderStartFailure {
    error: Option<io::Error>,
    file: Option<FileCapability>,
    expected: Option<FileRevision>,
    max_bytes: u64,
}

impl_redacted_debug!(FileRevisionReaderStartFailure);

impl FileRevisionReaderStartFailure {
    fn new(
        error: io::Error,
        file: FileCapability,
        expected: FileRevision,
        max_bytes: u64,
    ) -> Self {
        Self {
            error: Some(error),
            file: Some(file),
            expected: Some(expected),
            max_bytes,
        }
    }

    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("reader start failure retains its error")
    }

    pub fn retry(mut self) -> Result<FileRevisionReader, Self> {
        let file = self
            .file
            .take()
            .expect("reader start failure retains its file");
        let expected = self
            .expected
            .take()
            .expect("reader start failure retains its revision");
        file.into_revision_reader(expected, self.max_bytes)
    }

    pub fn into_parts(mut self) -> (io::Error, FileCapability, FileRevision, u64) {
        let error = self
            .error
            .take()
            .expect("reader start failure retains its error");
        let file = self
            .file
            .take()
            .expect("reader start failure retains its file");
        let expected = self
            .expected
            .take()
            .expect("reader start failure retains its revision");
        (error, file, expected, self.max_bytes)
    }
}

impl Drop for FileRevisionReaderStartFailure {
    fn drop(&mut self) {
        if self.file.is_some() || self.expected.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "file revision reader finish failures retain the armed reader and must be retried or unpacked"]
pub struct FileRevisionReaderFinishFailure {
    error: Option<io::Error>,
    reader: Option<FileRevisionReader>,
}

impl_redacted_debug!(FileRevisionReaderFinishFailure);

impl FileRevisionReaderFinishFailure {
    pub fn error(&self) -> &io::Error {
        self.error
            .as_ref()
            .expect("reader finish failure retains its error")
    }

    pub fn retry(mut self) -> Result<FileCapability, Self> {
        self.reader
            .take()
            .expect("reader finish failure retains its reader")
            .finish()
    }

    pub fn into_reader(mut self) -> FileRevisionReader {
        self.reader
            .take()
            .expect("reader finish failure retains its reader")
    }
}

impl Drop for FileRevisionReaderFinishFailure {
    fn drop(&mut self) {
        if self.reader.is_some() {
            std::process::abort();
        }
    }
}

impl_redacted_debug!(FileRevision);
impl_redacted_debug!(FileRevisionObservation);

impl FileRevision {
    pub fn retained(&self) -> Self {
        Self {
            authority: self.authority.clone(),
            identity: self.identity,
            size: self.size,
            stamp: self.stamp,
        }
    }

    pub fn observation(&self) -> FileRevisionObservation {
        FileRevisionObservation {
            authority: self.authority.clone(),
            identity: self.identity,
            size: self.size,
            stamp: self.stamp,
        }
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn modified_at_ns(&self) -> io::Result<u64> {
        platform::file_modified_at_ns(self.stamp)
    }

    pub fn changed_at_ns(&self) -> io::Result<u64> {
        platform::file_changed_at_ns(self.stamp)
    }
}

pub struct ExpectedFileContent {
    revision: FileRevision,
    sha256: [u8; 32],
}

impl_redacted_debug!(ExpectedFileContent);

impl ExpectedFileContent {
    pub fn new(revision: FileRevision, sha256: [u8; 32]) -> Self {
        Self { revision, sha256 }
    }
}

pub struct FileParkRequest {
    file: FileCapability,
    expected: ExpectedFileContent,
}

impl_redacted_debug!(FileParkRequest);

#[must_use = "file park source classification must be handled"]
pub enum FileParkRequestSource {
    Current(FileParkRequest),
    Displaced,
}

impl_redacted_debug!(FileParkRequestSource);

#[must_use = "failed file park source classification retains its exact request"]
pub struct FileParkRequestSourceError {
    error: io::Error,
    request: FileParkRequest,
}

impl_redacted_debug!(FileParkRequestSourceError);

impl FileParkRequestSourceError {
    pub fn into_parts(self) -> (io::Error, FileParkRequest) {
        (self.error, self.request)
    }
}

impl FileParkRequest {
    fn validate_revision(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.file
            .validate_revision_in(operation, &self.expected.revision)
    }

    pub fn classify_source(
        self,
        parent: &Directory,
    ) -> Result<FileParkRequestSource, FileParkRequestSourceError> {
        if self.file.parent.inner.authority.as_ptr() != parent.inner.authority.as_ptr()
            || self.file.parent.inner.identity != parent.inner.identity
        {
            return Err(FileParkRequestSourceError {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file park request belongs to another directory",
                ),
                request: self,
            });
        }
        let current = match parent.open_file(&self.file.name) {
            Ok(current) => current,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Ok(FileParkRequestSource::Displaced);
            }
            Err(error) => {
                return Err(FileParkRequestSourceError {
                    error,
                    request: self,
                });
            }
        };
        let revision = match current.revision() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(FileParkRequestSourceError {
                    error,
                    request: self,
                });
            }
        };
        if current.identity != self.file.identity
            || revision.authority.as_ptr() != self.expected.revision.authority.as_ptr()
            || revision.identity != self.expected.revision.identity
            || revision.size != self.expected.revision.size
            || revision.stamp != self.expected.revision.stamp
        {
            return Ok(FileParkRequestSource::Displaced);
        }
        let digest = {
            use sha2::{Digest, Sha256};

            let mut reader = match current.reader(self.expected.revision.size) {
                Ok(reader) => reader,
                Err(error) => {
                    return Err(FileParkRequestSourceError {
                        error,
                        request: self,
                    });
                }
            };
            let mut hasher = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = match reader.read(&mut buffer) {
                    Ok(read) => read,
                    Err(error) => {
                        return Err(FileParkRequestSourceError {
                            error,
                            request: self,
                        });
                    }
                };
                if read == 0 {
                    break;
                }
                hasher.update(&buffer[..read]);
            }
            if let Err(error) = reader.finish() {
                return Err(FileParkRequestSourceError {
                    error,
                    request: self,
                });
            }
            <[u8; 32]>::from(hasher.finalize())
        };
        let after = match current.revision() {
            Ok(revision) => revision,
            Err(error) => {
                return Err(FileParkRequestSourceError {
                    error,
                    request: self,
                });
            }
        };
        if after.authority.as_ptr() != self.expected.revision.authority.as_ptr()
            || after.identity != self.expected.revision.identity
            || after.size != self.expected.revision.size
            || after.stamp != self.expected.revision.stamp
            || digest != self.expected.sha256
        {
            return Ok(FileParkRequestSource::Displaced);
        }
        Ok(FileParkRequestSource::Current(self))
    }
}

impl fmt::Debug for FileCapability {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("FileCapability")
            .finish_non_exhaustive()
    }
}

impl FileCapability {
    pub fn same_file(&self, other: &Self) -> io::Result<bool> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        other.validate(&operation)?;
        if !Weak::ptr_eq(&self.authority, &other.authority) {
            return Err(stale_capability());
        }
        Ok(self.identity == other.identity)
    }

    pub fn move_no_replace(
        self,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> FileMoveOutcome {
        let source_parent = self.parent.clone();
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => return FileMoveOutcome::NoEffect { error, file: self },
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return FileMoveOutcome::NoEffect { error, file: self },
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| destination.validate(&operation))
        {
            return FileMoveOutcome::NoEffect { error, file: self };
        }
        if !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority) {
            return FileMoveOutcome::NoEffect {
                error: stale_capability(),
                file: self,
            };
        }
        if source_parent.inner.identity == destination.inner.identity
            && platform::leaf_names_equal(self.name.as_os_str(), destination_name.as_os_str())
        {
            return FileMoveOutcome::NoEffect {
                error: io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "file move destination matches its source",
                ),
                file: self,
            };
        }
        let mut token = match MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: source_parent.clone(),
                name: self.name.clone(),
            },
            NamespaceLeaf {
                parent: destination.clone(),
                name: destination_name.clone(),
            },
            None,
        ) {
            Ok(token) => token,
            Err(error) => return FileMoveOutcome::NoEffect { error, file: self },
        };
        let effect = platform::move_file_no_replace(
            &source_parent.inner.handle,
            self.name.as_os_str(),
            &self.handle,
            &destination.inner.handle,
            destination_name.as_os_str(),
        );
        let reported_success = effect.is_ok();
        match settle_file_move(
            self,
            destination,
            destination_name,
            reported_success,
            &mut token,
        ) {
            Ok((true, file)) => FileMoveOutcome::Applied(file),
            Ok((false, file)) => FileMoveOutcome::NoEffect {
                error: effect.err().unwrap_or_else(|| {
                    io::Error::other("file move reported no effect after native success")
                }),
                file,
            },
            Err(file) => FileMoveOutcome::AppliedUnverified(FileMoveObligation {
                error: effect
                    .err()
                    .unwrap_or_else(|| io::Error::other("file move could not be classified")),
                file: Some(file),
                destination: destination.clone(),
                destination_name: destination_name.clone(),
                reported_success,
                token,
            }),
        }
    }

    pub fn revision(&self) -> io::Result<FileRevision> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let (size, stamp) = platform::file_receipt_fields(&self.handle)?;
        self.validate(&operation)?;
        Ok(FileRevision {
            authority: self.authority.clone(),
            identity: self.identity,
            size,
            stamp,
        })
    }

    pub fn park_request(self, expected: ExpectedFileContent) -> FileParkRequest {
        FileParkRequest {
            file: self,
            expected,
        }
    }

    pub fn validate_revision(&self, expected: &FileRevision) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)
    }

    pub fn validate_revision_observation(
        &self,
        expected: &FileRevisionObservation,
    ) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let receipt = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || receipt != (expected.size, expected.stamp)
        {
            return Err(identity_changed("file revision observation changed"));
        }
        self.validate(&operation)
    }

    fn validate_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &FileRevision,
    ) -> io::Result<()> {
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let receipt = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || receipt != (expected.size, expected.stamp)
        {
            return Err(identity_changed("file revision changed"));
        }
        Ok(())
    }

    pub fn read_range_bounded(
        &self,
        expected: &FileRevision,
        offset: u64,
        length: usize,
    ) -> io::Result<Vec<u8>> {
        if length > MAX_FILE_RANGE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file range exceeds the supported bound",
            ));
        }
        let length_u64 = u64::try_from(length)
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "file range is too large"))?;
        let end = offset
            .checked_add(length_u64)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "file range overflowed"))?;
        if end > expected.size {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file range exceeds its expected revision",
            ));
        }

        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        self.validate_revision_in(&operation, expected)?;
        let mut bytes = vec![0_u8; length];
        let mut cursor = offset;
        let mut written = 0_usize;
        while cursor < end {
            let read = platform::read_at(&self.handle, &mut bytes[written..], cursor)?;
            if read == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "file ended before the requested range completed",
                ));
            }
            cursor = cursor
                .checked_add(u64::try_from(read).map_err(|_| {
                    io::Error::new(io::ErrorKind::InvalidData, "file read count is too large")
                })?)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file cursor overflowed"))?;
            written = written.checked_add(read).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "file result length overflowed")
            })?;
        }
        if cursor != end || bytes.len() != length || written != length {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file range did not complete exactly",
            ));
        }
        self.validate_revision_in(&operation, expected)?;
        self.validate(&operation)?;
        Ok(bytes)
    }

    pub fn into_revision_reader(
        self,
        expected: FileRevision,
        max_bytes: u64,
    ) -> Result<FileRevisionReader, FileRevisionReaderStartFailure> {
        if expected.size > max_bytes {
            return Err(FileRevisionReaderStartFailure::new(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file revision exceeds its reader bound",
                ),
                self,
                expected,
                max_bytes,
            ));
        }
        let authority = match self.parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return Err(FileRevisionReaderStartFailure::new(
                    error, self, expected, max_bytes,
                ));
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return Err(FileRevisionReaderStartFailure::new(
                    error, self, expected, max_bytes,
                ));
            }
        };
        if let Err(error) = self
            .validate(&operation)
            .and_then(|_| self.validate_revision_in(&operation, &expected))
        {
            drop(operation);
            return Err(FileRevisionReaderStartFailure::new(
                error, self, expected, max_bytes,
            ));
        }
        Ok(FileRevisionReader {
            state: Some(FileRevisionReaderState {
                file: self,
                expected,
                position: 0,
                operation,
            }),
        })
    }

    pub fn reader(&self, max_bytes: u64) -> io::Result<FileReader<'_>> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        Ok(FileReader {
            file: self,
            operation,
            position: 0,
            max_bytes,
        })
    }

    pub fn read_bounded(&self, max_bytes: u64) -> io::Result<Vec<u8>> {
        let mut reader = self.reader(max_bytes)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        reader.finish()?;
        Ok(bytes)
    }

    fn new(
        handle: File,
        identity: platform::Identity,
        parent: Directory,
        name: LeafName,
        authority: Weak<CapabilityAuthority>,
    ) -> Self {
        Self {
            handle,
            identity,
            parent,
            name,
            authority,
        }
    }

    fn validate(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.parent.validate(operation)?;
        if platform::file_identity(&self.handle)? != self.identity
            || platform::file_binding_state(
                &self.parent.inner.handle,
                self.name.as_os_str(),
                self.identity,
            )? != platform::BindingState::Exact
        {
            return Err(identity_changed("file capability changed binding"));
        }
        Ok(())
    }

    fn validate_bound_to(
        &self,
        parent: &Directory,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        self.validate(operation)?;
        parent.validate(operation)?;
        if self.parent.inner.identity == parent.inner.identity {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file capability belongs to another directory",
            ))
        }
    }
}

impl Read for FileRevisionReader {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        let state = self
            .state
            .as_mut()
            .expect("file revision reader retains armed state");
        state.file.validate(&state.operation)?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        if bytes.is_empty() || state.position == state.expected.size {
            return Ok(0);
        }
        let remaining = state
            .expected
            .size
            .checked_sub(state.position)
            .ok_or_else(|| io::Error::other("file revision reader position overflowed"))?;
        let allowed = usize::try_from(remaining.min(bytes.len() as u64))
            .map_err(|_| io::Error::other("file revision read length does not fit this platform"))?;
        let read = platform::read_at(&state.file.handle, &mut bytes[..allowed], state.position)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file ended before its admitted revision",
            ));
        }
        let position = state
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file revision reader position overflowed"))?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        state.file.validate(&state.operation)?;
        state.position = position;
        Ok(read)
    }
}

impl Seek for FileRevisionReader {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        let state = self
            .state
            .as_mut()
            .expect("file revision reader retains armed state");
        state.file.validate(&state.operation)?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        let next = match position {
            SeekFrom::Start(position) => i128::from(position),
            SeekFrom::End(delta) => i128::from(state.expected.size) + i128::from(delta),
            SeekFrom::Current(delta) => i128::from(state.position) + i128::from(delta),
        };
        if !(0..=i128::from(state.expected.size)).contains(&next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "file revision reader seek escaped its admitted range",
            ));
        }
        let position = u64::try_from(next)
            .map_err(|_| io::Error::other("file revision reader position overflowed"))?;
        state
            .file
            .validate_revision_in(&state.operation, &state.expected)?;
        state.file.validate(&state.operation)?;
        state.position = position;
        Ok(position)
    }
}

impl FileRevisionReader {
    pub fn finish(mut self) -> Result<FileCapability, FileRevisionReaderFinishFailure> {
        let validation = {
            let state = self
                .state
                .as_ref()
                .expect("file revision reader retains armed state");
            state
                .file
                .validate(&state.operation)
                .and_then(|_| {
                    state
                        .file
                        .validate_revision_in(&state.operation, &state.expected)
                })
                .and_then(|_| state.file.validate(&state.operation))
        };
        if let Err(error) = validation {
            return Err(FileRevisionReaderFinishFailure {
                error: Some(error),
                reader: Some(self),
            });
        }
        let FileRevisionReaderState {
            file,
            expected: _,
            position: _,
            operation,
        } = self
            .state
            .take()
            .expect("file revision reader retains armed state");
        drop(operation);
        Ok(file)
    }

    pub fn cancel(mut self) -> (FileCapability, FileRevision) {
        let FileRevisionReaderState {
            file,
            expected,
            position: _,
            operation,
        } = self
            .state
            .take()
            .expect("file revision reader retains armed state");
        drop(operation);
        (file, expected)
    }
}

impl Drop for FileRevisionReader {
    fn drop(&mut self) {
        if self.state.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "file readers must call finish to prove EOF and final binding; Drop only cancels the read"]
pub struct FileReader<'a> {
    file: &'a FileCapability,
    operation: CapabilityOperation,
    position: u64,
    max_bytes: u64,
}

impl fmt::Debug for FileReader<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("FileReader").finish_non_exhaustive()
    }
}

impl FileReader<'_> {
    pub fn finish(self) -> io::Result<()> {
        let mut probe = [0_u8; 1];
        if platform::read_at(&self.file.handle, &mut probe, self.position)? != 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "file capability was not read to completion",
            ));
        }
        self.file.validate(&self.operation)
    }
}

impl Read for FileReader<'_> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        if self.position == self.max_bytes {
            let mut probe = [0_u8; 1];
            return match platform::read_at(&self.file.handle, &mut probe, self.position)? {
                0 => Ok(0),
                _ => Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "file capability exceeded its read bound",
                )),
            };
        }
        let allowed = usize::try_from((self.max_bytes - self.position).min(bytes.len() as u64))
            .map_err(|_| io::Error::other("file read bound does not fit this platform"))?;
        let read = platform::read_at(&self.file.handle, &mut bytes[..allowed], self.position)?;
        self.position = self
            .position
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file read offset overflowed"))?;
        Ok(read)
    }
}

#[must_use = "a staged file must be sealed, discarded, or retained as an obligation"]
pub struct StagedFile {
    file: FileCapability,
    token: StageToken,
}

impl fmt::Debug for StagedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("StagedFile").finish_non_exhaustive()
    }
}

impl StagedFile {
    pub fn writer(&mut self) -> io::Result<StagedWriter<'_>> {
        let authority = self.file.parent.authority()?;
        let operation = authority.enter()?;
        self.file.validate(&operation)?;
        self.file.handle.set_len(0)?;
        Ok(StagedWriter {
            staged: self,
            operation,
            position: 0,
        })
    }

    pub fn write_all(&mut self, bytes: &[u8]) -> io::Result<()> {
        let mut writer = self.writer()?;
        writer.write_all(bytes)?;
        writer.finish()
    }

    pub fn seal(self) -> Result<SealedStagedFile, StageSealFailure> {
        let authority = match self.file.parent.authority() {
            Ok(authority) => authority,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        if let Err(error) = self.file.validate(&operation) {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.file.handle.sync_all() {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.file.validate(&operation) {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.token.update(StageRegistryPhase::Sealed) {
            return Err(StageSealFailure::new(error, self));
        }
        Ok(SealedStagedFile {
            file: self.file,
            token: self.token,
        })
    }

    pub fn discard(self) -> StageDiscardOutcome {
        let Self { file, mut token } = self;
        drop(file);
        match token.discard() {
            Ok(()) => StageDiscardOutcome::Discarded,
            Err(error) => StageDiscardOutcome::AppliedUnverified(StageDiscardObligation {
                error,
                token: Some(token),
            }),
        }
    }
}

#[must_use = "stage seal failures retain the staged file"]
pub struct StageSealFailure {
    error: io::Error,
    staged: Option<StagedFile>,
}

impl fmt::Debug for StageSealFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StageSealFailure")
            .finish_non_exhaustive()
    }
}

impl StageSealFailure {
    fn new(error: io::Error, staged: StagedFile) -> Self {
        Self {
            error,
            staged: Some(staged),
        }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn into_staged(mut self) -> StagedFile {
        self.staged.take().expect("failed stage is retained")
    }
}

#[must_use = "a sealed stage must be published, discarded, or retained as an obligation"]
pub struct SealedStagedFile {
    file: FileCapability,
    token: StageToken,
}

impl fmt::Debug for SealedStagedFile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SealedStagedFile")
            .finish_non_exhaustive()
    }
}

impl SealedStagedFile {
    pub fn discard(self) -> StageDiscardOutcome {
        StagedFile {
            file: self.file,
            token: self.token,
        }
        .discard()
    }
}

#[must_use = "stage discard effects must be explicitly settled"]
#[derive(Debug)]
pub enum StageDiscardOutcome {
    Discarded,
    AppliedUnverified(StageDiscardObligation),
}

#[must_use = "stage discard resolutions must be handled"]
#[derive(Debug)]
pub enum StageDiscardResolution {
    Discarded,
    Indeterminate(StageDiscardObligation),
}

#[must_use = "stage discard obligations must be reconciled"]
pub struct StageDiscardObligation {
    error: io::Error,
    token: Option<StageToken>,
}

impl_redacted_debug!(StageDiscardObligation);

impl StageDiscardObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> StageDiscardResolution {
        let mut token = self.token.take().expect("discard obligation retains token");
        match token.discard() {
            Ok(()) => StageDiscardResolution::Discarded,
            Err(_) => {
                self.token = Some(token);
                StageDiscardResolution::Indeterminate(self)
            }
        }
    }
}

impl SealedStagedFile {
    pub fn replace_nondurable(self, destination: ReplaceDestination) -> FileReplaceOutcome {
        match destination {
            ReplaceDestination::Vacant { parent, name } => {
                let fallback = ReplaceDestination::Vacant {
                    parent: parent.clone(),
                    name: name.clone(),
                };
                continue_file_replace(self, &parent, &name, None, fallback, None)
            }
            ReplaceDestination::Existing(request) => {
                let receipt = ExpectedContentReceipt::capture(&request);
                let parent = request.file.parent.clone();
                let name = request.file.name.clone();
                match parent.park_file(request) {
                    FileParkOutcome::Parked(displaced) => continue_file_replace(
                        self,
                        &parent,
                        &name,
                        Some(displaced),
                        ReplaceDestination::Vacant {
                            parent: parent.clone(),
                            name: name.clone(),
                        },
                        Some(receipt),
                    ),
                    FileParkOutcome::NoEffect { error, request } => {
                        FileReplaceOutcome::NoEffect {
                            error,
                            staged: self,
                            destination: ReplaceDestination::Existing(request),
                        }
                    }
                    FileParkOutcome::AppliedUnverified(park) => {
                        FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                            error: io::Error::other(
                                "replacement destination park is not yet settled",
                            ),
                            state: Some(FileReplaceObligationState::Parking {
                                park,
                                staged: self,
                                receipt,
                            }),
                        })
                    }
                }
            }
        }
    }

    pub fn promote_no_replace(
        mut self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
    ) -> FilePromotionOutcome {
        let authority = match source_parent.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FilePromotionOutcome::NoEffect {
                    error,
                    staged: self,
                };
            }
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(error) => {
                return FilePromotionOutcome::NoEffect {
                    error,
                    staged: self,
                };
            }
        };
        if let Err(error) = self.file.validate_bound_to(source_parent, &operation) {
            return FilePromotionOutcome::NoEffect {
                error,
                staged: self,
            };
        }
        if let Err(error) = destination_parent.validate(&operation) {
            return FilePromotionOutcome::NoEffect {
                error,
                staged: self,
            };
        }
        if !Weak::ptr_eq(
            &source_parent.inner.authority,
            &destination_parent.inner.authority,
        ) {
            return FilePromotionOutcome::NoEffect {
                error: stale_capability(),
                staged: self,
            };
        }
        if let Err(error) = self
            .token
            .prepare_promotion(destination_parent, destination_name)
        {
            return FilePromotionOutcome::NoEffect {
                error,
                staged: self,
            };
        }

        let rename = platform::rename_no_replace(
            &source_parent.inner.handle,
            self.file.name.as_os_str(),
            &self.file.handle,
            &destination_parent.inner.handle,
            destination_name.as_os_str(),
        );
        let source = platform::file_binding_state(
            &source_parent.inner.handle,
            self.file.name.as_os_str(),
            self.file.identity,
        );
        let destination = platform::file_binding_state(
            &destination_parent.inner.handle,
            destination_name.as_os_str(),
            self.file.identity,
        );

        match (rename, source, destination) {
            (_, Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
                if let Err(error) = sync_rename_parents(source_parent, destination_parent) {
                    return FilePromotionOutcome::AppliedUnverified(FilePromotionObligation {
                        error,
                        retained: self,
                        destination: destination_parent.clone(),
                        destination_name: destination_name.clone(),
                    });
                }
                match platform::open_file(
                    &destination_parent.inner.handle,
                    destination_name.as_os_str(),
                ) {
                    Ok(handle)
                        if platform::file_identity(&handle).ok() == Some(self.file.identity) =>
                    {
                        let applied = FileCapability::new(
                            handle,
                            self.file.identity,
                            destination_parent.clone(),
                            destination_name.clone(),
                            self.file.authority.clone(),
                        );
                        if applied.validate(&operation).is_err()
                            || self.token.disarm().is_err()
                        {
                            FilePromotionOutcome::AppliedUnverified(FilePromotionObligation {
                                error: identity_changed(
                                    "promoted file lost its authority chain",
                                ),
                                retained: self,
                                destination: destination_parent.clone(),
                                destination_name: destination_name.clone(),
                            })
                        } else {
                            FilePromotionOutcome::Applied(applied)
                        }
                    }
                    Ok(_) => FilePromotionOutcome::AppliedUnverified(
                        FilePromotionObligation {
                            error: identity_changed(
                                "promoted file changed before read capability admission",
                            ),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                        },
                    ),
                    Err(error) => FilePromotionOutcome::AppliedUnverified(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                        },
                    ),
                }
            }
            (Err(error), Ok(platform::BindingState::Exact), _) => {
                match self.token.update(StageRegistryPhase::Sealed) {
                    Ok(()) => FilePromotionOutcome::NoEffect {
                        error,
                        staged: self,
                    },
                    Err(update) => FilePromotionOutcome::AppliedUnverified(
                        FilePromotionObligation {
                            error: io::Error::other(format!(
                                "promotion failed and stage registry could not settle: {error}; {update}"
                            )),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                        },
                    ),
                }
            }
            (Ok(()), _, _) => {
                FilePromotionOutcome::AppliedUnverified(FilePromotionObligation {
                    error: identity_changed("file promotion effect could not be verified"),
                    retained: self,
                    destination: destination_parent.clone(),
                    destination_name: destination_name.clone(),
                })
            }
            (Err(error), _, _) => {
                FilePromotionOutcome::AppliedUnverified(FilePromotionObligation {
                    error,
                    retained: self,
                    destination: destination_parent.clone(),
                    destination_name: destination_name.clone(),
                })
            }
        }
    }
}

fn continue_file_replace(
    staged: SealedStagedFile,
    parent: &Directory,
    name: &LeafName,
    displaced: Option<ParkedFile>,
    fallback: ReplaceDestination,
    receipt: Option<ExpectedContentReceipt>,
) -> FileReplaceOutcome {
    let source = staged.file.parent.clone();
    match staged.promote_no_replace(&source, parent, name) {
        FilePromotionOutcome::Applied(current) => FileReplaceOutcome::Replaced {
            current,
            displaced,
        },
        FilePromotionOutcome::NoEffect { error, staged } => {
            let Some(displaced) = displaced else {
                return FileReplaceOutcome::NoEffect {
                    error,
                    staged,
                    destination: fallback,
                };
            };
            restore_displaced_after_failed_replace(
                staged,
                displaced,
                receipt.expect("existing replacement retains its receipt"),
                error,
            )
        }
        FilePromotionOutcome::AppliedUnverified(promotion) => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error: io::Error::other("replacement promotion is not yet settled"),
                state: Some(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                }),
            })
        }
    }
}

fn restore_displaced_after_failed_replace(
    staged: SealedStagedFile,
    displaced: ParkedFile,
    receipt: ExpectedContentReceipt,
    promotion_error: io::Error,
) -> FileReplaceOutcome {
    match displaced.restore() {
        FileRestoreOutcome::Restored(file) => FileReplaceOutcome::NoEffect {
            error: promotion_error,
            staged,
            destination: ReplaceDestination::Existing(receipt.rebuild(file)),
        },
        FileRestoreOutcome::NoEffect { error, parked } => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error,
                state: Some(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                }),
            })
        }
        FileRestoreOutcome::AppliedUnverified(restore) => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error: io::Error::other("replacement rollback is not yet settled"),
                state: Some(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                }),
            })
        }
    }
}

fn settle_file_replace(
    state: FileReplaceObligationState,
) -> Result<FileReplaceResolution, FileReplaceObligationState> {
    match state {
        FileReplaceObligationState::Parking {
            park,
            staged,
            receipt,
        } => {
            let (parent, name) = {
                let request = park.request.as_ref().expect("park obligation retains request");
                (request.file.parent.clone(), request.file.name.clone())
            };
            match park.reconcile() {
                FileParkResolution::Parked(displaced) => replace_outcome_to_resolution(
                    continue_file_replace(
                        staged,
                        &parent,
                        &name,
                        Some(displaced),
                        ReplaceDestination::Vacant {
                            parent: parent.clone(),
                            name: name.clone(),
                        },
                        Some(receipt),
                    ),
                ),
                FileParkResolution::NoEffect(request) => Ok(FileReplaceResolution::NoEffect {
                    staged,
                    destination: ReplaceDestination::Existing(request),
                }),
                FileParkResolution::Indeterminate(park) => {
                    Err(FileReplaceObligationState::Parking {
                        park,
                        staged,
                        receipt,
                    })
                }
            }
        }
        FileReplaceObligationState::Promoting {
            promotion,
            displaced,
            fallback,
            receipt,
        } => match promotion.reconcile() {
            FilePromotionResolution::Applied(current) => Ok(FileReplaceResolution::Replaced {
                current,
                displaced,
            }),
            FilePromotionResolution::NoEffect(staged) => {
                let Some(displaced) = displaced else {
                    return Ok(FileReplaceResolution::NoEffect {
                        staged,
                        destination: fallback,
                    });
                };
                replace_outcome_to_resolution(restore_displaced_after_failed_replace(
                    staged,
                    displaced,
                    receipt.expect("existing replacement retains its receipt"),
                    io::Error::other("replacement promotion had no effect"),
                ))
            }
            FilePromotionResolution::Indeterminate(promotion) => {
                Err(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                })
            }
        },
        FileReplaceObligationState::RestoreParked {
            parked,
            staged,
            receipt,
        } => match parked.restore() {
            FileRestoreOutcome::Restored(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: ReplaceDestination::Existing(receipt.rebuild(file)),
            }),
            FileRestoreOutcome::NoEffect { parked, .. } => {
                Err(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                })
            }
            FileRestoreOutcome::AppliedUnverified(restore) => {
                Err(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                })
            }
        },
        FileReplaceObligationState::RestoreObligation {
            restore,
            staged,
            receipt,
        } => match restore.reconcile() {
            FileRestoreResolution::Restored(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: ReplaceDestination::Existing(receipt.rebuild(file)),
            }),
            FileRestoreResolution::NoEffect(parked) => {
                Err(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                })
            }
            FileRestoreResolution::Indeterminate(restore) => {
                Err(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                })
            }
        },
    }
}

fn replace_outcome_to_resolution(
    outcome: FileReplaceOutcome,
) -> Result<FileReplaceResolution, FileReplaceObligationState> {
    match outcome {
        FileReplaceOutcome::Replaced { current, displaced } => {
            Ok(FileReplaceResolution::Replaced { current, displaced })
        }
        FileReplaceOutcome::NoEffect {
            staged,
            destination,
            ..
        } => Ok(FileReplaceResolution::NoEffect {
            staged,
            destination,
        }),
        FileReplaceOutcome::AppliedUnverified(mut obligation) => Err(obligation
            .state
            .take()
            .expect("replace obligation retains state")),
    }
}

pub struct StagedWriter<'a> {
    staged: &'a mut StagedFile,
    operation: CapabilityOperation,
    position: u64,
}

impl fmt::Debug for StagedWriter<'_> {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StagedWriter")
            .finish_non_exhaustive()
    }
}

impl StagedWriter<'_> {
    pub fn finish(self) -> io::Result<()> {
        self.staged.file.handle.sync_all()?;
        self.staged.file.validate(&self.operation)
    }
}

impl Write for StagedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let written = platform::write_at(&self.staged.file.handle, bytes, self.position)?;
        self.position = self
            .position
            .checked_add(written as u64)
            .ok_or_else(|| io::Error::other("staged file write offset overflowed"))?;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.staged.file.handle.sync_data()
    }
}

#[must_use = "a root session must be explicitly revoked or transferred into reset"]
pub struct RootSession {
    root_identity: DirectoryIdentity,
    authority: Arc<CapabilityAuthority>,
}

struct SessionDrainRecoveryState {
    files: Vec<ParkedFile>,
    directories: Vec<ParkedDirectory>,
    directory_create_preservations: Vec<DirectoryCreatePreservation>,
    file_removals: Vec<FileRemovalObligation>,
    file_restores: Vec<FileRestoreObligation>,
    directory_removals: Vec<DirectoryRemovalObligation>,
    directory_restores: Vec<DirectoryRestoreObligation>,
}

impl_redacted_debug!(SessionDrainRecoveryState);

#[derive(Clone, Copy)]
enum DrainRecoveryDecision {
    Remove,
    Restore,
}

impl SessionDrainRecoveryState {
    fn file_count(&self) -> usize {
        self.files
            .len()
            .saturating_add(self.file_removals.len())
            .saturating_add(self.file_restores.len())
    }

    fn directory_count(&self) -> usize {
        self.directories
            .len()
            .saturating_add(self.directory_removals.len())
            .saturating_add(self.directory_restores.len())
    }

    fn preserved_directory_create_count(&self) -> usize {
        self.directory_create_preservations.len()
    }

    fn is_empty(&self) -> bool {
        self.files.is_empty()
            && self.directories.is_empty()
            && self.directory_create_preservations.is_empty()
            && self.file_removals.is_empty()
            && self.file_restores.is_empty()
            && self.directory_removals.is_empty()
            && self.directory_restores.is_empty()
    }

    fn settle(&mut self, permits: &[DrainRecoveryPermit], decision: DrainRecoveryDecision) {
        let permit_by_park = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();

        for obligation in std::mem::take(&mut self.file_removals) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("file removal recovery retains parked file")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(token_id)) else {
                self.file_removals.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                FileRemovalResolution::Removed => {}
                FileRemovalResolution::NoEffect(parked) => {
                    self.files.push(parked);
                }
                FileRemovalResolution::Indeterminate(obligation) => {
                    self.file_removals.push(obligation);
                }
            };
        }
        for obligation in std::mem::take(&mut self.file_restores) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("file restore recovery retains parked file")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(token_id)) else {
                self.file_restores.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                FileRestoreResolution::Restored(_) => {}
                FileRestoreResolution::NoEffect(parked) => {
                    self.files.push(parked);
                }
                FileRestoreResolution::Indeterminate(obligation) => {
                    self.file_restores.push(obligation);
                }
            };
        }

        for parked in std::mem::take(&mut self.files) {
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(parked.token.id)) else {
                self.files.push(parked);
                continue;
            };
            match decision {
                DrainRecoveryDecision::Remove => match parked.remove_with_recovery(permit) {
                    FileRemovalOutcome::Removed => {}
                    FileRemovalOutcome::NoEffect { parked, .. } => self.files.push(parked),
                    FileRemovalOutcome::AppliedUnverified(obligation) => {
                        self.file_removals.push(obligation);
                    }
                },
                DrainRecoveryDecision::Restore => match parked.restore_with_recovery(permit) {
                    FileRestoreOutcome::Restored(_) => {}
                    FileRestoreOutcome::NoEffect { parked, .. } => self.files.push(parked),
                    FileRestoreOutcome::AppliedUnverified(obligation) => {
                        self.file_restores.push(obligation);
                    }
                },
            }
        }

        for obligation in std::mem::take(&mut self.directory_removals) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("directory removal recovery retains parked directory")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(token_id)) else {
                self.directory_removals.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                DirectoryRemovalResolution::Removed => {}
                DirectoryRemovalResolution::NoEffect(parked) => {
                    self.directories.push(parked);
                }
                DirectoryRemovalResolution::Indeterminate(obligation) => {
                    self.directory_removals.push(obligation);
                }
            };
        }
        for obligation in std::mem::take(&mut self.directory_restores) {
            let token_id = obligation
                .parked
                .as_ref()
                .expect("directory restore recovery retains parked directory")
                .token
                .id;
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(token_id)) else {
                self.directory_restores.push(obligation);
                continue;
            };
            match obligation.reconcile_with_recovery(permit) {
                DirectoryRestoreResolution::Restored(_) => {}
                DirectoryRestoreResolution::NoEffect(parked) => {
                    self.directories.push(parked);
                }
                DirectoryRestoreResolution::Indeterminate(obligation) => {
                    self.directory_restores.push(obligation);
                }
            };
        }

        for parked in std::mem::take(&mut self.directories) {
            let Some(permit) = permit_by_park
                .get(&DrainRecoveryParkId::Directory(parked.token.id))
            else {
                self.directories.push(parked);
                continue;
            };
            match decision {
                DrainRecoveryDecision::Remove => {
                    match parked.remove_empty_with_recovery(permit) {
                        DirectoryRemovalOutcome::Removed => {}
                        DirectoryRemovalOutcome::NoEffect { parked, .. } => {
                            self.directories.push(parked);
                        }
                        DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
                            self.directory_removals.push(obligation);
                        }
                    }
                }
                DrainRecoveryDecision::Restore => match parked.restore_with_recovery(permit) {
                    DirectoryRestoreOutcome::Restored(_) => {}
                    DirectoryRestoreOutcome::NoEffect { parked, .. } => {
                        self.directories.push(parked);
                    }
                    DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                        self.directory_restores.push(obligation);
                    }
                },
            }
        }
    }

    fn acknowledge_preserved_directory_creates(&mut self, permits: &[DrainRecoveryPermit]) {
        let permit_by_effect = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();
        for preservation in std::mem::take(&mut self.directory_create_preservations) {
            let token_id = preservation.token.id;
            let Some(permit) =
                permit_by_effect.get(&DrainRecoveryParkId::DirectoryCreate(token_id))
            else {
                self.directory_create_preservations.push(preservation);
                continue;
            };
            if let Err(preservation) = preservation.acknowledge_preserved_with_recovery(permit) {
                self.directory_create_preservations.push(preservation);
            }
        }
    }

    fn acknowledge_external_directory_creates(&mut self, permits: &[DrainRecoveryPermit]) {
        let permit_by_effect = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();
        for preservation in std::mem::take(&mut self.directory_create_preservations) {
            let token_id = preservation.token.id;
            let Some(permit) =
                permit_by_effect.get(&DrainRecoveryParkId::DirectoryCreate(token_id))
            else {
                self.directory_create_preservations.push(preservation);
                continue;
            };
            if !permit
                .authority
                .directory_create_is_external(&preservation.token)
            {
                self.directory_create_preservations.push(preservation);
                continue;
            }
            if let Err(preservation) = preservation.acknowledge_preserved_with_recovery(permit) {
                self.directory_create_preservations.push(preservation);
            }
        }
    }

    fn transfer_preserved_directory_creates_to_reset(
        &mut self,
        permits: &[DrainRecoveryPermit],
    ) {
        let permit_by_effect = permits
            .iter()
            .map(|permit| (permit.park_token_id, permit))
            .collect::<HashMap<_, _>>();
        for preservation in std::mem::take(&mut self.directory_create_preservations) {
            let token_id = preservation.token.id;
            let Some(permit) =
                permit_by_effect.get(&DrainRecoveryParkId::DirectoryCreate(token_id))
            else {
                self.directory_create_preservations.push(preservation);
                continue;
            };
            if let Err(preservation) = preservation.transfer_to_reset(permit) {
                self.directory_create_preservations.push(preservation);
            }
        }
    }

}

impl fmt::Debug for RootSession {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("RootSession").finish_non_exhaustive()
    }
}

impl RootSession {
    pub fn acquire(path: &Path) -> RootSessionAcquireOutcome {
        let process_image = match capture_process_image_ancestry() {
            Ok(process_image) => process_image,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(
                    RootSessionError::ProcessImage(error),
                );
            }
        };
        match platform::open_or_create_root(path) {
            Ok(construction) => {
                try_acquire_lease_and_finish_root(construction, process_image)
            }
            Err(error) => {
                let (error, construction) = error.into_parts();
                match construction {
                    Some(construction) => RootSessionAcquireOutcome::AppliedUnverified(
                        RootSessionAcquireObligation {
                            error: RootSessionError::Create(error),
                            construction: Some(construction),
                            lease: None,
                            process_image: Some(process_image),
                        },
                    ),
                    None => RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error)),
                }
            }
        }
    }

    fn acquire_absolute_directory_guard(
        guard: &platform::AbsoluteDirectoryGuard,
    ) -> RootSessionAcquireOutcome {
        let process_image = match capture_process_image_ancestry() {
            Ok(process_image) => process_image,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(
                    RootSessionError::ProcessImage(error),
                );
            }
        };
        let construction = match platform::root_construction_from_absolute_directory_guard(guard) {
            Ok(construction) => construction,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error));
            }
        };
        try_acquire_lease_and_finish_root(construction, process_image)
    }

    pub fn identity(&self) -> DirectoryIdentity {
        self.root_identity
    }

    pub fn root(&self) -> io::Result<Directory> {
        let operation = self.authority.enter()?;
        Ok(Directory::from_handle(
            platform::clone_root(&self.authority.root)?,
            self.root_identity,
            Arc::downgrade(&self.authority),
            None,
        ))
        .and_then(|root| {
            root.validate(&operation)?;
            Ok(root)
        })
    }

    pub fn admit_absolute_directory_authority(
        &self,
        path: &Path,
    ) -> io::Result<AdmittedAbsoluteDirectory> {
        let directory = self.admit_absolute_directory_capability(path)?;
        let admitted = AdmittedAbsoluteDirectory {
            inner: Arc::new(AdmittedAbsoluteDirectoryInner { directory }),
        };
        admitted.revalidate()?;
        Ok(admitted)
    }

    pub fn admit_absolute_directory_authority_outside_root(
        &self,
        path: &Path,
    ) -> AbsoluteDirectoryOutsideRootAdmission {
        let admitted = match self.admit_absolute_directory_authority(path) {
            Ok(admitted) => admitted,
            Err(error) => {
                return AbsoluteDirectoryOutsideRootAdmission::Unavailable(error);
            }
        };
        let Some(guard) = admitted
            .inner
            .directory
            .inner
            .absolute_ancestry
            .as_ref()
        else {
            return AbsoluteDirectoryOutsideRootAdmission::Unavailable(io::Error::other(
                "absolute directory admission lost its ancestry",
            ));
        };
        match platform::absolute_directory_is_outside_root(guard, &self.authority.root) {
            Ok(true) => AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted),
            Ok(false) => AbsoluteDirectoryOutsideRootAdmission::InsideRoot,
            Err(error) => AbsoluteDirectoryOutsideRootAdmission::Unavailable(error),
        }
    }

    pub fn admit_root_child_directory_authority(
        &self,
        directory: Directory,
        name: &LeafName,
    ) -> io::Result<AdmittedAbsoluteDirectory> {
        let operation = self.authority.enter()?;
        directory.validate(&operation)?;
        let identity = directory.inner.identity;
        let ancestry = platform::absolute_directory_guard_from_root_child(
            &self.authority.root,
            name.as_os_str(),
            &directory.inner.handle,
            identity.physical,
        )?;
        let handle = platform::clone_absolute_directory_guard(&ancestry)?;
        let admitted = AdmittedAbsoluteDirectory {
            inner: Arc::new(AdmittedAbsoluteDirectoryInner {
                directory: Directory::from_absolute_handle(
                    handle,
                    identity,
                    Arc::downgrade(&self.authority),
                    ancestry,
                ),
            }),
        };
        admitted.revalidate()?;
        Ok(admitted)
    }

    pub fn admit_absolute_directory(&self, path: &Path) -> io::Result<Directory> {
        self.admit_absolute_directory_capability(path)
    }

    fn admit_absolute_directory_capability(&self, path: &Path) -> io::Result<Directory> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external directory is not absolute",
            ));
        }
        let operation = self.authority.enter()?;
        let ancestry: platform::AbsoluteDirectoryGuard =
            platform::open_absolute_directory_guard(path)?;
        let identity = platform::absolute_directory_identity(&ancestry);
        let handle = platform::clone_absolute_directory_guard(&ancestry)?;
        let directory = Directory::from_absolute_handle(
            handle,
            self.authority.identity(identity),
            Arc::downgrade(&self.authority),
            ancestry,
        );
        directory.validate(&operation)?;
        Ok(directory)
    }

    pub fn validate_absolute_directory_outside_root(&self, path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "external directory is not absolute",
            ));
        }
        let _operation = self.authority.enter()?;
        let ancestry = platform::open_absolute_directory_guard(path)?;
        platform::validate_absolute_directory_outside_root(&ancestry, &self.authority.root)
    }

    pub fn validate_reset_preflight(&self) -> io::Result<()> {
        self.authority
            .validate_retained_process_image_outside_root()?;
        platform::validate_lease(&self.authority.lease)?;
        platform::validate_root(&self.authority.root)
    }

    pub fn validate_retained_authority(&self) -> io::Result<()> {
        platform::validate_lease(&self.authority.lease)?;
        platform::validate_root_handle(&self.authority.root)
    }

    pub fn revoke(self) -> RootRevokeOutcome {
        let start = self.authority.begin_terminal_drain();
        match start {
            Ok(()) => RootRevokeDrain {
                session: Some(self),
            }
            .try_settle(),
            Err(error) => {
                RootRevokeOutcome::Refused(RootRevokeStartFailure::new(self, error))
            }
        }
    }

    pub fn begin_reset(self) -> ResetStartOutcome {
        if let Err(error) = self
            .authority
            .validate_retained_process_image_outside_root()
        {
            return ResetStartOutcome::Refused(ResetStartFailure::new(self, error));
        }
        let start = self.authority.begin_terminal_drain();
        match start {
            Ok(()) => ResetDrainAuthority {
                session: Some(self),
            }
            .try_settle(),
            Err(error) => ResetStartOutcome::Refused(ResetStartFailure::new(self, error)),
        }
    }
}

fn capture_process_image_ancestry() -> io::Result<platform::ProcessImageAncestry> {
    let executable = std::env::current_exe()?;
    platform::capture_process_image_ancestry(&executable)
}

fn try_acquire_lease_and_finish_root(
    construction: platform::RootConstruction,
    process_image: platform::ProcessImageAncestry,
) -> RootSessionAcquireOutcome {
    let identity = match platform::root_construction_identity(&construction) {
        Ok(identity) => identity,
        Err(error) => {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Create(error),
                    construction: Some(construction),
                    lease: None,
                    process_image: Some(process_image),
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Open(error))
            };
        }
    };
    let root = match platform::root_construction_guard(&construction) {
        Ok(root) => root,
        Err(error) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Create(error),
                construction: Some(construction),
                lease: None,
                process_image: Some(process_image),
            });
        }
    };
    let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
    let lease = match platform::try_acquire_lease(root, lease_name.as_os_str()) {
        platform::LeaseAcquisitionOutcome::Acquired(lease) => lease,
        platform::LeaseAcquisitionOutcome::NoEffect(error)
            if error.kind() == io::ErrorKind::WouldBlock =>
        {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Busy,
                    construction: Some(construction),
                    lease: None,
                    process_image: Some(process_image),
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
            };
        }
        platform::LeaseAcquisitionOutcome::NoEffect(error) => {
            return if platform::root_construction_has_effect(&construction) {
                RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                    error: RootSessionError::Lease(error),
                    construction: Some(construction),
                    lease: None,
                    process_image: Some(process_image),
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Lease(error))
            };
        }
        platform::LeaseAcquisitionOutcome::AppliedUnverified(lease) => {
            let error = RootSessionError::Lease(copy_io_error(
                platform::lease_acquisition_error(&lease),
            ));
            return RootSessionAcquireOutcome::AppliedUnverified(
                RootSessionAcquireObligation {
                    error,
                    construction: Some(construction),
                    lease: Some(lease),
                    process_image: Some(process_image),
                },
            );
        }
    };
    finish_root_session(construction, identity, lease, process_image)
}

fn finish_root_session(
    construction: platform::RootConstruction,
    identity: platform::Identity,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
) -> RootSessionAcquireOutcome {
    use rand::RngCore;

    let root = platform::finish_root_construction(construction);
    let mut session_nonce = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut session_nonce);
    RootSessionAcquireOutcome::Acquired(RootSession {
        root_identity: DirectoryIdentity {
            session: session_nonce,
            physical: identity,
        },
        authority: Arc::new(CapabilityAuthority {
            operations: Mutex::new(OperationState {
                phase: AUTHORITY_LIVE,
                active: 0,
                outstanding_effects: 0,
                next_move_id: 1,
                moves: HashMap::new(),
                next_effect_owner_id: 1,
                effect_owner_handles: HashMap::new(),
                active_effect_owners: HashMap::new(),
                next_stage_id: 1,
                stages: HashMap::new(),
                next_stage_create_id: 1,
                stage_creations: HashMap::new(),
                next_directory_create_id: 1,
                directory_creations: HashMap::new(),
                next_file_park_id: 1,
                file_parks: HashMap::new(),
                file_parks_checked_out: 0,
                next_directory_park_id: 1,
                directory_parks: HashMap::new(),
                directory_parks_checked_out: 0,
                park_owners: HashMap::new(),
                next_transient_id: 1,
                transients: HashMap::new(),
            }),
            session_nonce,
            root,
            lease,
            process_image,
        }),
    })
}

impl Drop for RootSession {
    fn drop(&mut self) {
        let phase = match self.authority.operations.lock() {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        }
        .phase;
        if phase == AUTHORITY_LIVE && self.authority.begin_terminal_drain().is_err() {
            std::process::abort();
        }
        let state = match self.authority.operations.lock() {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        };
        if state.active != 0
            || state.outstanding_effects != 0
            || !state.moves.is_empty()
            || state.file_parks_checked_out != 0
            || state.directory_parks_checked_out != 0
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !state.directory_creations.is_empty()
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.park_owners.is_empty()
            || !state.transients.is_empty()
            || !state.active_effect_owners.is_empty()
            || matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_QUIESCING)
        {
            std::process::abort();
        }
    }
}

#[must_use = "revocation outcome retains linear session authority until it is terminal"]
#[derive(Debug)]
pub enum RootRevokeOutcome {
    Revoked,
    Pending(RootRevokeDrain),
    Recovery {
        recovery: RootRevokeRecovery,
    },
    Refused(RootRevokeStartFailure),
    Failed(RootRevokeDrainFailure),
}

#[must_use = "revocation drain must be settled or transferred with recovery authority"]
pub struct RootRevokeDrain {
    session: Option<RootSession>,
}

impl_redacted_debug!(RootRevokeDrain);

impl RootRevokeDrain {
    pub fn try_settle(mut self) -> RootRevokeOutcome {
        let session = self.session.take().expect("revoke drain retains session");
        let authority = session.authority.clone();
        let settlement = authority.try_finish_terminal_drain(AUTHORITY_REVOKED, false);
        match settlement {
            Ok(SessionDrainSettlement::Ready) => RootRevokeOutcome::Revoked,
            Ok(SessionDrainSettlement::Pending) => {
                self.session = Some(session);
                RootRevokeOutcome::Pending(self)
            }
            Ok(SessionDrainSettlement::Recovery { recovery, permits }) => {
                self.session = Some(session);
                RootRevokeOutcome::Recovery {
                    recovery: RootRevokeRecovery {
                        drain: self,
                        recovery,
                        permits,
                    },
                }
            }
            Err(error) => {
                self.session = Some(session);
                RootRevokeOutcome::Failed(RootRevokeDrainFailure::new(self, error))
            }
        }
    }
}

#[must_use = "revocation recovery must settle every abandoned effect before the drain can finish"]
pub struct RootRevokeRecovery {
    drain: RootRevokeDrain,
    recovery: SessionDrainRecoveryState,
    permits: Vec<DrainRecoveryPermit>,
}

impl_redacted_debug!(RootRevokeRecovery);

impl RootRevokeRecovery {
    pub fn file_count(&self) -> usize {
        self.recovery.file_count()
    }

    pub fn directory_count(&self) -> usize {
        self.recovery.directory_count()
    }

    pub fn preserved_directory_create_count(&self) -> usize {
        self.recovery.preserved_directory_create_count()
    }

    pub fn acknowledge_preserved_directory_creates(mut self) -> RootRevokeOutcome {
        self.recovery
            .acknowledge_preserved_directory_creates(&self.permits);
        self.finish()
    }

    pub fn restore_all(mut self) -> RootRevokeOutcome {
        self.recovery
            .settle(&self.permits, DrainRecoveryDecision::Restore);
        self.finish()
    }

    pub fn remove_all(mut self) -> RootRevokeOutcome {
        self.recovery
            .settle(&self.permits, DrainRecoveryDecision::Remove);
        self.finish()
    }

    fn finish(self) -> RootRevokeOutcome {
        if self.recovery.is_empty() {
            self.drain.try_settle()
        } else {
            RootRevokeOutcome::Recovery {
                recovery: self,
            }
        }
    }
}

#[must_use = "revocation start refusal retains the live session and must be retried"]
pub struct RootRevokeStartFailure {
    error: io::Error,
    session: Option<RootSession>,
}

impl_redacted_debug!(RootRevokeStartFailure);

impl RootRevokeStartFailure {
    fn new(session: RootSession, error: io::Error) -> Self {
        Self {
            error,
            session: Some(session),
        }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn retry(mut self) -> RootRevokeOutcome {
        self.session
            .take()
            .expect("revocation start refusal retains session")
            .revoke()
    }
}

#[must_use = "revocation drain failure retains the draining authority and must be retried"]
pub struct RootRevokeDrainFailure {
    error: io::Error,
    drain: Option<RootRevokeDrain>,
}

impl_redacted_debug!(RootRevokeDrainFailure);

impl RootRevokeDrainFailure {
    fn new(drain: RootRevokeDrain, error: io::Error) -> Self {
        Self {
            error,
            drain: Some(drain),
        }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn retry(mut self) -> RootRevokeOutcome {
        self.drain
            .take()
            .expect("revocation failure retains drain")
            .try_settle()
    }
}

#[must_use = "reset outcome retains linear session authority until it is terminal"]
#[derive(Debug)]
pub enum ResetStartOutcome {
    Ready(RootResetAuthority),
    Pending(ResetDrainAuthority),
    Recovery {
        recovery: ResetDrainRecovery,
    },
    Refused(ResetStartFailure),
    Failed(ResetDrainFailure),
}

#[must_use = "reset drain must be settled or transferred with recovery authority"]
pub struct ResetDrainAuthority {
    session: Option<RootSession>,
}

impl_redacted_debug!(ResetDrainAuthority);

impl ResetDrainAuthority {
    pub fn try_settle(mut self) -> ResetStartOutcome {
        let session = self.session.take().expect("reset drain retains session");
        let authority = session.authority.clone();
        let settlement = authority.try_finish_terminal_drain(AUTHORITY_RESETTING, true);
        match settlement {
            Ok(SessionDrainSettlement::Ready) => ResetStartOutcome::Ready(RootResetAuthority {
                session: Some(session),
            }),
            Ok(SessionDrainSettlement::Pending) => {
                self.session = Some(session);
                ResetStartOutcome::Pending(self)
            }
            Ok(SessionDrainSettlement::Recovery { recovery, permits }) => {
                self.session = Some(session);
                ResetStartOutcome::Recovery {
                    recovery: ResetDrainRecovery {
                        drain: self,
                        recovery,
                        permits,
                    },
                }
            }
            Err(error) => {
                self.session = Some(session);
                ResetStartOutcome::Failed(ResetDrainFailure::new(self, error))
            }
        }
    }

    fn cancel_reset(mut self) -> RootRevokeOutcome {
        let session = self.session.take().expect("reset drain retains session");
        let cancellation = session
            .authority
            .cancel_reset_pending_directory_creates();
        let revoke = RootRevokeDrain {
            session: Some(session),
        };
        match cancellation {
            Ok(()) => revoke.try_settle(),
            Err(error) => {
                RootRevokeOutcome::Failed(RootRevokeDrainFailure::new(revoke, error))
            }
        }
    }
}

#[must_use = "reset recovery must settle every abandoned effect before the drain can finish"]
pub struct ResetDrainRecovery {
    drain: ResetDrainAuthority,
    recovery: SessionDrainRecoveryState,
    permits: Vec<DrainRecoveryPermit>,
}

impl_redacted_debug!(ResetDrainRecovery);

impl ResetDrainRecovery {
    pub fn file_count(&self) -> usize {
        self.recovery.file_count()
    }

    pub fn directory_count(&self) -> usize {
        self.recovery.directory_count()
    }

    pub fn unsettled_external_or_deferred_count(&self) -> usize {
        self.recovery.preserved_directory_create_count()
    }

    pub fn defer_managed_reset(mut self) -> ResetStartOutcome {
        self.recovery
            .transfer_preserved_directory_creates_to_reset(&self.permits);
        self.finish()
    }

    pub fn acknowledge_external(mut self) -> ResetStartOutcome {
        self.recovery
            .acknowledge_external_directory_creates(&self.permits);
        self.finish()
    }

    pub fn restore_all(mut self) -> ResetStartOutcome {
        self.recovery
            .settle(&self.permits, DrainRecoveryDecision::Restore);
        self.finish()
    }

    pub fn remove_all(mut self) -> ResetStartOutcome {
        self.recovery
            .settle(&self.permits, DrainRecoveryDecision::Remove);
        self.finish()
    }

    fn finish(self) -> ResetStartOutcome {
        if self.recovery.is_empty() {
            self.drain.try_settle()
        } else {
            ResetStartOutcome::Recovery {
                recovery: self,
            }
        }
    }
}

#[must_use = "reset start failure retains the sole session and must be retried or explicitly cancelled"]
pub struct ResetStartFailure {
    error: io::Error,
    session: Option<RootSession>,
}

impl fmt::Debug for ResetStartFailure {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ResetStartFailure")
            .finish_non_exhaustive()
    }
}

impl ResetStartFailure {
    fn new(session: RootSession, error: io::Error) -> Self {
        Self {
            error,
            session: Some(session),
        }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn retry(mut self) -> ResetStartOutcome {
        self.session
            .take()
            .expect("reset start refusal retains session")
            .begin_reset()
    }

    pub fn cancel_reset(mut self) -> RootSession {
        self.session
            .take()
            .expect("reset start refusal retains session")
    }
}

impl Drop for ResetStartFailure {
    fn drop(&mut self) {
        if self.session.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "reset drain failure retains authority and must be retried or cancelled into revocation"]
pub struct ResetDrainFailure {
    error: io::Error,
    drain: Option<ResetDrainAuthority>,
}

impl_redacted_debug!(ResetDrainFailure);

impl ResetDrainFailure {
    fn new(drain: ResetDrainAuthority, error: io::Error) -> Self {
        Self {
            error,
            drain: Some(drain),
        }
    }

    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn retry(mut self) -> ResetStartOutcome {
        self.drain
            .take()
            .expect("reset failure retains drain")
            .try_settle()
    }

    pub fn cancel_reset(mut self) -> RootRevokeOutcome {
        self.drain
            .take()
            .expect("reset failure retains drain")
            .cancel_reset()
    }
}

impl Drop for ResetDrainFailure {
    fn drop(&mut self) {
        if self.drain.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "reset authority must be explicitly cleared, preserved, or released"]
pub struct RootResetAuthority {
    session: Option<RootSession>,
}

#[must_use = "root clear outcomes retain reset authority until deletion is proven"]
#[derive(Debug)]
pub enum RootClearOutcome {
    Cleared(RootClearReceipt),
    Failed(RootClearFailure),
}

#[must_use = "a cleared root receipt must release the retained root session and lease"]
pub struct RootClearReceipt {
    authority: Option<RootResetAuthority>,
}

impl_redacted_debug!(RootClearReceipt);

impl RootClearReceipt {
    pub fn root_identity(&self) -> DirectoryIdentity {
        self.authority
            .as_ref()
            .expect("clear receipt retains reset authority")
            .root_identity()
    }

    pub fn release(mut self) -> Result<(), Self> {
        let authority = self
            .authority
            .take()
            .expect("clear receipt retains reset authority");
        match authority.release() {
            Ok(()) => Ok(()),
            Err(authority) => {
                self.authority = Some(authority);
                Err(self)
            }
        }
    }
}

impl Drop for RootClearReceipt {
    fn drop(&mut self) {
        if self.authority.is_some() {
            std::process::abort();
        }
    }
}

#[must_use = "failed root clear authority must be retried or explicitly preserved"]
pub struct RootClearFailure {
    error: io::Error,
    authority: Option<RootResetAuthority>,
}

impl_redacted_debug!(RootClearFailure);

impl RootClearFailure {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn retry(mut self) -> RootClearOutcome {
        self.authority
            .take()
            .expect("root clear failure retains reset authority")
            .clear_root()
    }

    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let authority = self
            .authority
            .take()
            .expect("root clear failure retains reset authority");
        match authority.acknowledge_preserved_directory_creates() {
            Ok(()) => Ok(()),
            Err(authority) => {
                self.authority = Some(authority);
                Err(self)
            }
        }
    }
}

impl Drop for RootClearFailure {
    fn drop(&mut self) {
        if self.authority.is_some() {
            std::process::abort();
        }
    }
}

impl fmt::Debug for RootResetAuthority {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RootResetAuthority")
            .finish_non_exhaustive()
    }
}

impl RootResetAuthority {
    pub fn root_identity(&self) -> DirectoryIdentity {
        self.session
            .as_ref()
            .expect("reset session is retained")
            .root_identity
    }

    pub fn clear_root(mut self) -> RootClearOutcome {
        let result = (|| {
            let session = self
                .session
                .as_ref()
                .expect("reset session is retained");
            let operation = session.authority.enter_reset_operation()?;
            platform::validate_lease(&session.authority.lease)?;
            platform::validate_root(&session.authority.root)?;
            let lease_name = LeafName::new(ROOT_LEASE_NAME).expect("fixed lease name is valid");
            platform::validate_process_image_outside_root(
                &session.authority.process_image,
                &session.authority.root,
            )?;
            platform::clear_root_children(
                &session.authority.root,
                &session.authority.lease,
                lease_name.as_os_str(),
            )?;
            session
                .authority
                .retire_reset_pending_directory_creates(&operation)
        })();
        if let Err(error) = result {
            return RootClearOutcome::Failed(RootClearFailure {
                error,
                authority: Some(self),
            });
        }
        RootClearOutcome::Cleared(RootClearReceipt {
            authority: Some(self),
        })
    }

    pub fn acknowledge_preserved_directory_creates(mut self) -> Result<(), Self> {
        let session = self
            .session
            .as_ref()
            .expect("reset session is retained");
        let operation = match session.authority.enter_reset_operation() {
            Ok(operation) => operation,
            Err(_) => return Err(self),
        };
        if session
            .authority
            .retire_reset_pending_directory_creates(&operation)
            .is_err()
        {
            return Err(self);
        }
        drop(operation);
        self.revoke();
        drop(self.session.take());
        Ok(())
    }

    pub fn release(mut self) -> Result<(), Self> {
        if self
            .session
            .as_ref()
            .expect("reset session is retained")
            .authority
            .has_reset_pending_directory_creates()
        {
            return Err(self);
        }
        self.revoke();
        drop(self.session.take());
        Ok(())
    }

    fn revoke(&self) {
        if let Some(session) = &self.session {
            let mut state = session
                .authority
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            state.phase = AUTHORITY_REVOKED;
        }
    }
}

impl Drop for RootResetAuthority {
    fn drop(&mut self) {
        if self.session.is_some() {
            std::process::abort();
        }
    }
}

fn stale_capability() -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        "filesystem capability has been revoked",
    )
}

fn identity_changed(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

fn settle_file_move(
    file: FileCapability,
    destination: &Directory,
    destination_name: &LeafName,
    reported_success: bool,
    token: &mut MoveEffectToken,
) -> Result<(bool, FileCapability), FileCapability> {
    let authority = match file.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return Err(file),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return Err(file),
    };
    if destination.validate(&operation).is_err()
        || !Weak::ptr_eq(&file.parent.inner.authority, &destination.inner.authority)
        || platform::file_identity(&file.handle).ok() != Some(file.identity)
    {
        return Err(file);
    }
    let source = platform::file_binding_state(
        &file.parent.inner.handle,
        file.name.as_os_str(),
        file.identity,
    );
    let target = platform::file_binding_state(
        &destination.inner.handle,
        destination_name.as_os_str(),
        file.identity,
    );
    match classify_move_topology(reported_success, source.ok(), target.ok()) {
        MoveTopology::Applied => {
            if sync_rename_parents(&file.parent, destination).is_err()
                || destination.validate(&operation).is_err()
            {
                return Err(file);
            }
            let handle = match platform::open_file(
                &destination.inner.handle,
                destination_name.as_os_str(),
            ) {
                Ok(handle) if platform::file_identity(&handle).ok() == Some(file.identity) => {
                    handle
                }
                _ => return Err(file),
            };
            let moved = FileCapability::new(
                handle,
                file.identity,
                destination.clone(),
                destination_name.clone(),
                file.authority.clone(),
            );
            if moved.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(file);
            }
            Ok((true, moved))
        }
        MoveTopology::NoEffect => {
            if file.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(file);
            }
            Ok((false, file))
        }
        MoveTopology::Indeterminate => Err(file),
    }
}

fn settle_directory_move(
    directory: Directory,
    destination: &Directory,
    destination_name: &LeafName,
    reported_success: bool,
    token: &mut MoveEffectToken,
) -> Result<(bool, Directory), Directory> {
    let Some(binding) = directory.inner.parent.as_ref() else {
        return Err(directory);
    };
    let source_parent = binding.directory.clone();
    let source_name = binding.name.clone();
    let authority = match source_parent.authority() {
        Ok(authority) => authority,
        Err(_) => return Err(directory),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return Err(directory),
    };
    if destination.validate(&operation).is_err()
        || !Weak::ptr_eq(
            &source_parent.inner.authority,
            &destination.inner.authority,
        )
        || platform::directory_identity(&directory.inner.handle).ok()
            != Some(directory.inner.identity.physical)
    {
        return Err(directory);
    }
    let source = platform::directory_binding_state(
        &source_parent.inner.handle,
        &source_name,
        directory.inner.identity.physical,
    );
    let target = platform::directory_binding_state(
        &destination.inner.handle,
        destination_name.as_os_str(),
        directory.inner.identity.physical,
    );
    match classify_move_topology(reported_success, source.ok(), target.ok()) {
        MoveTopology::Applied => {
            if sync_rename_parents(&source_parent, destination).is_err()
                || destination.validate(&operation).is_err()
            {
                return Err(directory);
            }
            let (handle, identity) = match platform::open_directory(
                &destination.inner.handle,
                destination_name.as_os_str(),
            ) {
                Ok(opened) if opened.1 == directory.inner.identity.physical => opened,
                _ => return Err(directory),
            };
            let moved = Directory::from_handle(
                handle,
                authority.identity(identity),
                directory.inner.authority.clone(),
                Some(DirectoryParent {
                    directory: destination.clone(),
                    name: destination_name.as_os_str().to_os_string(),
                }),
            );
            if moved.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(directory);
            }
            Ok((true, moved))
        }
        MoveTopology::NoEffect => {
            if directory.validate(&operation).is_err() || token.settle(&operation).is_err() {
                return Err(directory);
            }
            Ok((false, directory))
        }
        MoveTopology::Indeterminate => Err(directory),
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MoveTopology {
    Applied,
    NoEffect,
    Indeterminate,
}

fn classify_move_topology(
    reported_success: bool,
    source: Option<platform::BindingState>,
    destination: Option<platform::BindingState>,
) -> MoveTopology {
    if source == Some(platform::BindingState::Absent)
        && destination == Some(platform::BindingState::Exact)
    {
        MoveTopology::Applied
    } else if !reported_success && source == Some(platform::BindingState::Exact) {
        MoveTopology::NoEffect
    } else {
        MoveTopology::Indeterminate
    }
}

fn sync_rename_parents(source: &Directory, destination: &Directory) -> io::Result<()> {
    platform::sync_directory(&destination.inner.handle)?;
    if !Arc::ptr_eq(&source.inner, &destination.inner) {
        platform::sync_directory(&source.inner.handle)?;
    }
    Ok(())
}

fn random_leaf(prefix: &str) -> LeafName {
    use rand::RngCore;

    let mut nonce = [0_u8; 16];
    rand::rngs::OsRng.fill_bytes(&mut nonce);
    LeafName::new(format!("{prefix}{}", encode_hex(&nonce)))
        .expect("generated filesystem leaf is valid")
}

fn hash_parked_file(
    file: &platform::FileCleanupHandle,
    size: u64,
) -> io::Result<[u8; 32]> {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    let mut offset = 0_u64;
    while offset < size {
        let mut chunk = [0_u8; 64 * 1024];
        let wanted = usize::try_from((size - offset).min(chunk.len() as u64))
            .map_err(|_| io::Error::other("file hash bound does not fit this platform"))?;
        let read = platform::read_parked_at(file, &mut chunk[..wanted], offset)?;
        if read == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "file ended before its authenticated size",
            ));
        }
        digest.update(&chunk[..read]);
        offset = offset
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file hash offset overflowed"))?;
    }
    let mut probe = [0_u8; 1];
    if platform::read_parked_at(file, &mut probe, size)? != 0 {
        return Err(identity_changed(
            "file exceeded its authenticated size during hashing",
        ));
    }
    Ok(digest.finalize().into())
}

fn verify_parked_file(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    expected_content: &ExpectedFileContent,
) -> io::Result<()> {
    verify_parked_revision(parent, park_name, parked, &expected_content.revision)?;
    if hash_parked_file(parked, expected_content.revision.size)? != expected_content.sha256
        || verify_parked_revision(parent, park_name, parked, &expected_content.revision).is_err()
    {
        return Err(identity_changed(
            "parked file did not match its expected content receipt",
        ));
    }
    Ok(())
}

fn verify_parked_revision(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    revision: &FileRevision,
) -> io::Result<()> {
    if platform::parked_file_receipt_fields(parked)? != (revision.size, revision.stamp)
        || platform::file_binding_state(
            &parent.inner.handle,
            park_name.as_os_str(),
            revision.identity,
        )? != platform::BindingState::Exact
    {
        return Err(identity_changed("parked file revision changed"));
    }
    Ok(())
}

fn finish_new_file_park(
    request: FileParkRequest,
    park_name: LeafName,
    mut token: FileParkRegistryToken,
    operation: &CapabilityOperation,
) -> FileParkOutcome {
    let parent = request.file.parent.clone();
    let original_name = request.file.name.clone();
    let identity = request.file.identity;
    let authority = operation.authority.clone();
    let mut guard = match authority.take_file_park(operation, &token) {
        Ok(guard) => guard,
        Err(error) => {
            return FileParkOutcome::AppliedUnverified(FileParkObligation {
                error,
                request: Some(request),
                token,
                park_name,
                phase: FileParkPhase::Parking,
                digest_verified: false,
            });
        }
    };
    match verify_parked_file(
        &parent,
        &park_name,
        &guard.record().cleanup,
        &request.expected,
    ) {
        Ok(()) if parent.validate(operation).is_ok() => {
            guard.record_mut().expected_digest = None;
            guard.record_mut().phase = FileParkRegistryPhase::Live;
            drop(guard);
            FileParkOutcome::Parked(ParkedFile {
                parent,
                original_name,
                park_name,
                identity,
                size: request.expected.revision.size,
                stamp: request.expected.revision.stamp,
                verified: true,
                token,
                authority: request.file.authority.clone(),
            })
        }
        Ok(()) => {
            guard.record_mut().expected_digest = None;
            drop(guard);
            FileParkOutcome::AppliedUnverified(FileParkObligation {
                error: identity_changed("file park lost its authority chain"),
                request: Some(request),
                token,
                park_name,
                phase: FileParkPhase::Parking,
                digest_verified: true,
            })
        }
        Err(error) => {
            let restoration = {
                let record = guard.record_mut();
                platform::restore_parked_file(
                    &record.parent.inner.handle,
                    record.name.as_os_str(),
                    &mut record.cleanup,
                    record.identity,
                    record.original_name.as_os_str(),
                )
            };
            match restoration {
            Ok(_) if request.file.validate(operation).is_ok() => {
                guard.disarm(&mut token, operation);
                FileParkOutcome::NoEffect { error, request }
            }
            Ok(_) => {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error: identity_changed(
                        "rejected file receipt was restored but not re-admitted",
                    ),
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::RestoringRejectedReceipt,
                    digest_verified: false,
                })
            }
            Err(restore) => {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error: io::Error::other(format!(
                        "file receipt was rejected and restoration was not proven: {error}; {restore}"
                    )),
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::RestoringRejectedReceipt,
                    digest_verified: false,
                })
            }
        }
        }
    }
}

fn settle_file_park(mut obligation: FileParkObligation, force_restore: bool) -> FileParkResolution {
    let request = obligation.request.as_ref().expect("park obligation retains request");
    let authority = match request.file.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    if request.file.parent.validate(&operation).is_err()
        || request.validate_revision(&operation).is_err()
    {
        return FileParkResolution::Indeterminate(obligation);
    }
    let mut guard = match authority.take_file_park(&operation, &obligation.token) {
        Ok(guard) => guard,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    let original = platform::file_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().original_name.as_os_str(),
        guard.record().identity,
    );
    let parked_state = platform::file_binding_state(
        &guard.record().parent.inner.handle,
        guard.record().name.as_os_str(),
        guard.record().identity,
    );
    match (original, parked_state) {
        (Ok(platform::BindingState::Exact), Ok(platform::BindingState::Absent)) => {
            guard.disarm(&mut obligation.token, &operation);
            FileParkResolution::NoEffect(obligation.request.take().expect("park request"))
        }
        (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
            if force_restore || obligation.phase == FileParkPhase::RestoringRejectedReceipt {
                let restoration = {
                    let record = guard.record_mut();
                    platform::restore_parked_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        &mut record.cleanup,
                        record.identity,
                        record.original_name.as_os_str(),
                    )
                };
                return match restoration {
                    Ok(_) if request.file.validate(&operation).is_ok() => {
                        guard.disarm(&mut obligation.token, &operation);
                        FileParkResolution::NoEffect(
                            obligation.request.take().expect("park request"),
                        )
                    }
                    _ => FileParkResolution::Indeterminate(obligation),
                };
            }
            if !obligation.digest_verified {
                if let Err(error) = verify_parked_file(
                    &request.file.parent,
                    &obligation.park_name,
                    &guard.record().cleanup,
                    &request.expected,
                ) {
                    obligation.error = error;
                    obligation.phase = FileParkPhase::RestoringRejectedReceipt;
                    drop(guard);
                    return settle_file_park(obligation, true);
                }
                obligation.digest_verified = true;
                guard.record_mut().expected_digest = None;
            }
            let request = obligation.request.take().expect("park request");
            if verify_parked_revision(
                &request.file.parent,
                &obligation.park_name,
                &guard.record().cleanup,
                &request.expected.revision,
            )
            .is_err()
            {
                obligation.request = Some(request);
                return FileParkResolution::Indeterminate(obligation);
            }
            if request.file.parent.validate(&operation).is_err() {
                obligation.request = Some(request);
                return FileParkResolution::Indeterminate(obligation);
            }
            guard.record_mut().phase = FileParkRegistryPhase::Live;
            drop(guard);
            FileParkResolution::Parked(ParkedFile {
                parent: request.file.parent.clone(),
                original_name: request.file.name.clone(),
                park_name: obligation.park_name,
                identity: request.file.identity,
                size: request.expected.revision.size,
                stamp: request.expected.revision.stamp,
                verified: true,
                token: obligation.token,
                authority: request.file.authority.clone(),
            })
        }
        _ => FileParkResolution::Indeterminate(obligation),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha2::{Digest, Sha256};

    fn acquire_test_root(path: &Path) -> RootSession {
        match RootSession::acquire(path) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            RootSessionAcquireOutcome::NoEffect(error) => {
                panic!("root acquisition had no effect: {error}")
            }
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                match obligation.reconcile() {
                    RootSessionAcquireOutcome::Acquired(session) => session,
                    RootSessionAcquireOutcome::NoEffect(error) => {
                        panic!("root acquisition reconciliation had no effect: {error}")
                    }
                    RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                        let error = obligation.error().to_string();
                        let _ = obligation.cleanup();
                        panic!("root acquisition remained indeterminate: {error}")
                    }
                }
            }
        }
    }

    #[test]
    fn admitted_absolute_directory_retains_one_physical_binding() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);
        {
            let admitted = session
                .admit_absolute_directory_authority(&library)
                .expect("admit library");
            assert_eq!(
                admitted.filesystem_identity().expect("admitted identity"),
                session
                    .admit_absolute_directory(&library)
                    .expect("repeat admission")
                    .identity()
                    .expect("repeat identity")
                    .filesystem_identity()
            );
            admitted.revalidate().expect("revalidate admission");
        }
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn admitted_absolute_directories_retain_distinct_physical_bindings() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        let unrelated = temporary.path().join("unrelated");
        for path in [&app_root, &library, &unrelated] {
            std::fs::create_dir(path).expect("create directory");
        }
        let session = acquire_test_root(&app_root);
        {
            let admitted = session
                .admit_absolute_directory_authority(&library)
                .expect("admit library");
            let unrelated = session
                .admit_absolute_directory_authority(&unrelated)
                .expect("admit unrelated directory");
            assert_ne!(
                admitted.filesystem_identity().expect("library identity"),
                unrelated.filesystem_identity().expect("unrelated identity")
            );
            admitted.revalidate().expect("original binding remains valid");
        }
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn admitted_absolute_directory_accepts_the_filesystem_root() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        std::fs::create_dir(&app_root).expect("create app root");
        let session = acquire_test_root(&app_root);

        let root = session
            .admit_absolute_directory(Path::new("/"))
            .expect("admit filesystem root");
        match session.admit_absolute_directory_authority_outside_root(Path::new("/")) {
            AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted) => drop(admitted),
            AbsoluteDirectoryOutsideRootAdmission::InsideRoot => {
                panic!("filesystem root is not inside the nested app root")
            }
            AbsoluteDirectoryOutsideRootAdmission::Unavailable(error) => {
                panic!("filesystem root authority unavailable: {error}")
            }
        }

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn admitted_absolute_directory_accepts_the_volume_root() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        std::fs::create_dir(&app_root).expect("create app root");
        let volume_root = temporary
            .path()
            .ancestors()
            .last()
            .expect("temporary path volume root");
        assert!(volume_root.is_absolute());
        let session = acquire_test_root(&app_root);

        let root = session
            .admit_absolute_directory(volume_root)
            .expect("admit volume root");
        match session.admit_absolute_directory_authority_outside_root(volume_root) {
            AbsoluteDirectoryOutsideRootAdmission::Admitted(admitted) => drop(admitted),
            AbsoluteDirectoryOutsideRootAdmission::InsideRoot => {
                panic!("volume root is not inside the nested app root")
            }
            AbsoluteDirectoryOutsideRootAdmission::Unavailable(error) => {
                panic!("volume root authority unavailable: {error}")
            }
        }

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn admitted_absolute_directory_rejects_a_case_alias_leaf() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        let alias = temporary.path().join("Library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);

        session
            .admit_absolute_directory_authority(&alias)
            .expect_err("case alias must not satisfy exact absolute admission");

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn admitted_absolute_directory_refreshes_exact_name_after_parent_change() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        let alias = temporary.path().join("Library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);
        let admitted = session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");

        std::fs::create_dir(temporary.path().join("sibling")).expect("create sibling");
        admitted
            .revalidate()
            .expect("refresh exact proof after sibling change");
        std::fs::rename(&library, &alias).expect("rename admitted leaf");
        admitted
            .revalidate()
            .expect_err("case alias must invalidate the refreshed exact proof");

        drop(admitted);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn admitted_absolute_directory_rejects_replaced_leaf_binding() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        let displaced = temporary.path().join("displaced-library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);
        {
            let admitted = session
                .admit_absolute_directory_authority(&library)
                .expect("admit library");
            std::fs::rename(&library, &displaced).expect("displace admitted library");
            std::fs::create_dir(&library).expect("create replacement library");
            assert!(admitted.revalidate().is_err());
        }
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn admitted_root_acquisition_never_creates_or_leases_a_path_replacement() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        let displaced = temporary.path().join("displaced-library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);
        {
            let admitted = session
                .admit_absolute_directory_authority(&library)
                .expect("admit library");
            std::fs::rename(&library, &displaced).expect("displace admitted library");

            assert!(admitted.acquire_root_session().is_err());
            assert!(!library.exists(), "acquisition recreated a missing path");

            std::fs::create_dir(&library).expect("create replacement library");
            assert!(admitted.acquire_root_session().is_err());
            assert!(
                !library.join(ROOT_LEASE_NAME).exists(),
                "acquisition touched the replacement directory"
            );
        }
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn admitted_root_session_clones_its_detached_root_capability() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let app_session = acquire_test_root(&app_root);
        let admitted = app_session
            .admit_absolute_directory_authority(&library)
            .expect("admit library");
        let expected = admitted
            .filesystem_identity()
            .expect("admitted identity");
        let admitted_session = match admitted
            .acquire_root_session()
            .expect("start admitted root acquisition")
        {
            AdmittedRootSessionAcquireOutcome::Acquired(session) => session,
            AdmittedRootSessionAcquireOutcome::NoEffect(error) => {
                panic!("admitted root acquisition failed: {error}")
            }
            AdmittedRootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                let error = obligation.error().to_string();
                assert!(
                    obligation.cleanup().is_ok(),
                    "admitted root acquisition cleanup remained unsettled"
                );
                panic!("admitted root acquisition was unverified: {error}");
            }
        };
        admitted_session
            .validate_retained_authority()
            .expect("retained admitted root authority");
        assert_eq!(
            admitted_session
                .root()
                .expect("clone detached root")
                .identity()
                .expect("detached root identity")
                .filesystem_identity(),
            expected
        );
        drop(admitted_session);
        assert!(matches!(
            acquire_test_root(&library).revoke(),
            RootRevokeOutcome::Revoked
        ));
        assert!(matches!(app_session.revoke(), RootRevokeOutcome::Revoked));
    }

    fn park_preservation_test_file(root: &Directory) -> ParkedFile {
        park_test_file(root, "record.bin", "record.preserved", b"payload")
    }

    fn park_test_file(
        root: &Directory,
        name: &str,
        park_name: &str,
        payload: &[u8],
    ) -> ParkedFile {
        let file = root
            .open_file(&LeafName::new(name).expect("record leaf"))
            .expect("record capability");
        let revision = file.revision().expect("record revision");
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let request = file.park_request(ExpectedFileContent::new(revision, digest));
        match root.park_file_as(
            request,
            LeafName::new(park_name).expect("preserved leaf"),
        ) {
            FileParkOutcome::Parked(parked) => parked,
            FileParkOutcome::NoEffect { error, .. } => {
                panic!("preservation park had no effect: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                panic!("preservation park was not verified: {}", obligation.error())
            }
        }
    }

    fn park_test_directory(
        root: &Directory,
        name: &str,
        park_name: &str,
    ) -> ParkedDirectory {
        let directory = root
            .open_directory(&LeafName::new(name).expect("directory leaf"))
            .expect("directory capability");
        match directory.park_as(LeafName::new(park_name).expect("park leaf")) {
            DirectoryParkOutcome::Parked(parked) => parked,
            DirectoryParkOutcome::NoEffect { error, .. } => {
                panic!("directory park had no effect: {error}")
            }
            DirectoryParkOutcome::AppliedUnverified(obligation) => {
                panic!("directory park was not verified: {}", obligation.error())
            }
        }
    }

    fn create_test_directories(root: &Path, relative: &[&str]) {
        for path in relative {
            std::fs::create_dir(root.join(path)).expect("test directory");
        }
    }

    fn test_sealed_stage(root: &Directory, bytes: &[u8]) -> SealedStagedFile {
        let mut staged = match root.create_stage() {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => panic!("stage creation failed: {error}"),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                panic!("stage creation was not verified: {}", obligation.error())
            }
        };
        staged.write_all(bytes).expect("stage bytes");
        staged.seal().expect("sealed stage")
    }

    fn discard_test_park_registration(mut parked: ParkedFile) {
        let authority = parked.authority().expect("park authority");
        let operation = authority
            .enter_file_park(&parked.token)
            .expect("park cleanup operation");
        let guard = authority
            .take_file_park(&operation, &parked.token)
            .expect("park cleanup guard");
        guard.disarm(&mut parked.token, &operation);
    }

    fn test_file_move_obligation(
        file: FileCapability,
        destination: Directory,
        destination_name: &str,
        reported_success: bool,
    ) -> FileMoveObligation {
        let authority = file.parent.authority().expect("move authority");
        let destination_name = LeafName::new(destination_name).expect("destination leaf");
        let token = {
            let operation = authority.enter().expect("move reservation operation");
            MoveEffectToken::reserve(
                &authority,
                &operation,
                NamespaceLeaf {
                    parent: file.parent.clone(),
                    name: file.name.clone(),
                },
                NamespaceLeaf {
                    parent: destination.clone(),
                    name: destination_name.clone(),
                },
                None,
            )
            .expect("move effect reservation")
        };
        FileMoveObligation {
            error: io::Error::other("test move requires settlement"),
            file: Some(file),
            destination,
            destination_name,
            reported_success,
            token,
        }
    }

    fn test_directory_move_obligation(
        directory: Directory,
        destination: Directory,
        destination_name: &str,
        reported_success: bool,
    ) -> DirectoryMoveObligation {
        let authority = directory.authority().expect("move authority");
        let binding = directory.inner.parent.as_ref().expect("non-root directory");
        let source_name = LeafName::new(binding.name.clone()).expect("source leaf");
        let destination_name = LeafName::new(destination_name).expect("destination leaf");
        let token = {
            let operation = authority.enter().expect("move reservation operation");
            MoveEffectToken::reserve(
                &authority,
                &operation,
                NamespaceLeaf {
                    parent: binding.directory.clone(),
                    name: source_name,
                },
                NamespaceLeaf {
                    parent: destination.clone(),
                    name: destination_name.clone(),
                },
                Some(directory.inner.identity.physical),
            )
            .expect("move effect reservation")
        };
        DirectoryMoveObligation {
            error: io::Error::other("test directory move requires settlement"),
            directory: Some(directory),
            destination,
            destination_name,
            reported_success,
            token,
        }
    }

    fn claim_no_effect(receipt: FileMoveReceipt) -> FileCapability {
        match receipt.claim() {
            FileMoveReceiptOutcome::NoEffect(file) => file,
            FileMoveReceiptOutcome::Applied(_) => panic!("test move unexpectedly applied"),
            FileMoveReceiptOutcome::Pending(_) => panic!("test move remained pending"),
        }
    }

    #[test]
    fn effect_owner_rejects_sibling_anchor_and_returns_the_move_carrier() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("first")).expect("first anchor");
        std::fs::create_dir(temporary.path().join("second")).expect("second anchor");
        std::fs::write(temporary.path().join("second/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let first = root
            .open_directory(&LeafName::new("first").expect("first leaf"))
            .expect("first capability");
        let second = root
            .open_directory(&LeafName::new("second").expect("second leaf"))
            .expect("second capability");
        let first_owner = first.create_effect_owner().expect("first owner");
        let second_owner = second.create_effect_owner().expect("second owner");
        let file = second
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let obligation = test_file_move_obligation(file, second.clone(), "target.bin", false);

        let failure = first_owner
            .retain_file_move(obligation)
            .expect_err("sibling owner must reject the move");
        assert_eq!(failure.error().kind(), io::ErrorKind::PermissionDenied);
        let (_, obligation) = failure.into_parts();
        let receipt = second_owner
            .retain_file_move(obligation)
            .expect("correct owner retains returned carrier");
        second_owner.settle().expect("settle returned carrier");
        let file = claim_no_effect(receipt);
        assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
        assert!(second_owner.require_settled().is_ok());

        drop((file, first_owner, second_owner, first, second, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn dropped_move_receipts_remain_owned_until_explicit_settlement() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/before.bin"), b"before")
            .expect("before file");
        std::fs::write(temporary.path().join("domain/after.bin"), b"after")
            .expect("after file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");

        let before = domain
            .open_file(&LeafName::new("before.bin").expect("before leaf"))
            .expect("before capability");
        let before = owner
            .retain_file_move(test_file_move_obligation(
                before,
                domain.clone(),
                "before-target.bin",
                false,
            ))
            .expect("retain before-terminal receipt");
        drop(before);
        assert!(owner.require_settled().is_err());
        owner.settle().expect("settle abandoned pending receipt");
        assert!(owner.require_settled().is_ok());

        let after = domain
            .open_file(&LeafName::new("after.bin").expect("after leaf"))
            .expect("after capability");
        let after = owner
            .retain_file_move(test_file_move_obligation(
                after,
                domain.clone(),
                "after-target.bin",
                false,
            ))
            .expect("retain after-terminal receipt");
        owner.settle().expect("produce terminal result");
        assert!(owner.require_settled().is_err());
        drop(after);
        assert!(owner.require_settled().is_err());
        owner.settle().expect("dispose abandoned terminal result");
        assert!(owner.require_settled().is_ok());

        drop((owner, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn settlement_extraction_preserves_barriers_capacity_and_receipt_abandonment() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let first_file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let first = owner
            .retain_file_move(test_file_move_obligation(
                first_file,
                domain.clone(),
                "first-target.bin",
                false,
            ))
            .expect("retain first move");
        let extracted = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *owner
            .state
            .settlement_pause
            .lock()
            .expect("settlement pause") = Some(EffectOwnerSettlementPause {
            extracted: extracted.clone(),
            resume: resume.clone(),
        });
        let settling_owner = owner.clone();
        let settlement = std::thread::spawn(move || settling_owner.settle());
        extracted.wait();

        assert!(owner.has_pending());
        assert_eq!(
            owner
                .require_settled()
                .expect_err("in-flight settlement remains pending")
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        assert_eq!(
            owner
                .settle()
                .expect_err("a second settlement cannot overtake the first")
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        let authority = owner.state.authority.upgrade().expect("owner authority");
        {
            let state = authority.operations.lock().expect("operation state");
            assert!(state
                .active_effect_owners
                .get(&owner.state.id)
                .is_some_and(|active| Arc::ptr_eq(active, &owner.state)));
        }

        let mut queued = Vec::with_capacity(MAX_EFFECTS_PER_OWNER - 1);
        for index in 1..MAX_EFFECTS_PER_OWNER {
            let file = domain
                .open_file(&LeafName::new("source.bin").expect("source leaf"))
                .expect("source capability");
            queued.push(
                owner
                    .retain_file_move(test_file_move_obligation(
                        file,
                        domain.clone(),
                        &format!("queued-target-{index}.bin"),
                        false,
                    ))
                    .expect("in-flight capacity retains only the remaining permits"),
            );
        }
        let overflow_file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("overflow source capability");
        let failure = owner
            .retain_file_move(test_file_move_obligation(
                overflow_file,
                domain.clone(),
                "overflow-target.bin",
                false,
            ))
            .expect_err("in-flight effects count toward owner capacity");
        assert_eq!(failure.error().kind(), io::ErrorKind::WouldBlock);
        let (_, mut overflow) = failure.into_parts();
        let overflow_file = overflow.file.take().expect("returned overflow file");
        let operation = authority.enter().expect("overflow settlement operation");
        overflow
            .token
            .settle(&operation)
            .expect("settle returned overflow token");
        drop((operation, overflow_file, overflow));

        drop(first);
        for receipt in queued {
            drop(receipt);
        }
        resume.wait();
        settlement
            .join()
            .expect("settlement thread")
            .expect("first settlement");
        assert!(owner.has_pending());
        {
            let records = owner.state.effects.lock().expect("owner records");
            assert!(!records.settling);
            assert_eq!(records.in_flight, 0);
            assert_eq!(records.effects.len(), MAX_EFFECTS_PER_OWNER - 1);
        }
        owner.settle().expect("settle queued abandoned receipts");
        assert!(owner.require_settled().is_ok());

        drop((authority, owner, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn live_pending_move_receipt_blocks_terminal_drain_and_remains_claimable() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let receipt = owner
            .retain_file_move(test_file_move_obligation(
                file,
                domain.clone(),
                "target.bin",
                false,
            ))
            .expect("retain move");
        drop((owner, domain, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live pending receipt did not block revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        let file = claim_no_effect(receipt);
        assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
        drop(file);
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn live_terminal_move_receipt_blocks_drain_until_claimed() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let receipt = owner
            .retain_file_move(test_file_move_obligation(
                file,
                domain.clone(),
                "target.bin",
                false,
            ))
            .expect("retain move");
        owner.settle().expect("produce terminal result");
        drop((owner, domain, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live terminal receipt did not block revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        let file = claim_no_effect(receipt);
        assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
        drop(file);
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn dropped_terminal_move_receipt_is_reclaimed_during_drain() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let receipt = owner
            .retain_file_move(test_file_move_obligation(
                file,
                domain.clone(),
                "target.bin",
                false,
            ))
            .expect("retain move");
        owner.settle().expect("produce terminal result");
        drop(receipt);
        assert!(owner.require_settled().is_err());
        drop((owner, domain, root));

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn live_empty_effect_owner_blocks_terminal_drain_until_dropped() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        assert!(owner.require_settled().is_ok());
        drop((domain, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live empty owner did not block revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        drop(owner);
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn raw_file_and_directory_parks_refuse_drain_until_settled() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("record.bin"), b"payload")
            .expect("test file");
        std::fs::create_dir(temporary.path().join("folder")).expect("test directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked_file = park_preservation_test_file(&root);
        let parked_directory = park_test_directory(&root, "folder", "folder.parked");
        drop(root);

        let file_refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("raw file park did not refuse revocation: {outcome:?}"),
        };
        assert_eq!(file_refusal.error().kind(), io::ErrorKind::WouldBlock);
        parked_file
            .acknowledge_preserved()
            .expect("settle raw file park");

        let directory_refusal = match file_refusal.retry() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("raw directory park did not refuse revocation: {outcome:?}"),
        };
        assert_eq!(
            directory_refusal.error().kind(),
            io::ErrorKind::WouldBlock
        );
        let directory = match parked_directory.restore() {
            DirectoryRestoreOutcome::Restored(directory) => directory,
            DirectoryRestoreOutcome::NoEffect { error, .. } => {
                panic!("raw directory restore had no effect: {error}")
            }
            DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                panic!("raw directory restore was not verified: {}", obligation.error())
            }
        };
        drop(directory);
        assert!(matches!(
            directory_refusal.retry(),
            RootRevokeOutcome::Revoked
        ));
    }

    #[test]
    fn effect_owner_removes_a_nonempty_parked_directory_tree() {
        let temporary = tempfile::tempdir().expect("temporary root");
        create_test_directories(
            temporary.path(),
            &[
                "domain",
                "domain/victim",
                "domain/victim/nested",
                "domain/victim/nested/deeper",
                "domain/uncertain",
                "domain/uncertain/nested",
            ],
        );
        std::fs::write(
            temporary.path().join("domain/victim/nested/deeper/payload.bin"),
            b"payload",
        )
        .expect("nested payload");
        std::fs::write(
            temporary.path().join("domain/uncertain/nested/payload.bin"),
            b"payload",
        )
        .expect("uncertain payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let parked = park_test_directory(&domain, "victim", "victim.deleted");
        let uncertain = park_test_directory(&domain, "uncertain", "uncertain.deleted");

        owner
            .retain_parked_directory_tree_removal(RetainedDirectoryTreeRemoval::new(parked))
            .expect("retain tree removal");
        owner
            .retain_directory_tree_removal(DirectoryTreeRemovalObligation {
                error: io::Error::other("test tree removal requires reconciliation"),
                parked: Some(uncertain),
            })
            .expect("retain indeterminate tree removal");
        owner.settle().expect("settle tree removal");
        owner.require_settled().expect("tree removal settled");
        assert!(!temporary.path().join("domain/victim").exists());
        assert!(!temporary.path().join("domain/victim.deleted").exists());
        assert!(!temporary.path().join("domain/uncertain").exists());
        assert!(!temporary.path().join("domain/uncertain.deleted").exists());

        drop((owner, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn parked_tree_removal_preserves_the_recreated_canonical_binding() {
        let temporary = tempfile::tempdir().expect("temporary root");
        create_test_directories(temporary.path(), &["victim", "victim/nested"]);
        std::fs::write(temporary.path().join("victim/nested/old.bin"), b"old")
            .expect("old payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "victim", "victim.deleted");
        std::fs::create_dir(temporary.path().join("victim")).expect("replacement canonical root");
        std::fs::write(temporary.path().join("victim/new.bin"), b"new")
            .expect("replacement canonical payload");

        assert!(matches!(
            parked.remove_tree(),
            DirectoryTreeRemovalOutcome::Removed
        ));
        assert_eq!(
            std::fs::read(temporary.path().join("victim/new.bin"))
                .expect("replacement canonical payload"),
            b"new",
        );
        assert!(!temporary.path().join("victim.deleted").exists());

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn tree_removal_obligation_finishes_a_partially_cleared_root() {
        let temporary = tempfile::tempdir().expect("temporary root");
        create_test_directories(temporary.path(), &["victim", "victim/nested"]);
        std::fs::write(temporary.path().join("victim/removed.bin"), b"removed")
            .expect("first payload");
        std::fs::write(temporary.path().join("victim/nested/retained.bin"), b"retained")
            .expect("second payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "victim", "victim.deleted");
        std::fs::remove_file(temporary.path().join("victim.deleted/removed.bin"))
            .expect("simulate partial tree removal");
        let obligation = DirectoryTreeRemovalObligation {
            error: io::Error::other("test tree removal stopped after partial progress"),
            parked: Some(parked),
        };

        assert!(matches!(
            obligation.reconcile(),
            DirectoryTreeRemovalResolution::Removed
        ));
        assert!(!temporary.path().join("victim.deleted").exists());

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn parked_tree_destination_case_equivalent_collision_is_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary root");
        create_test_directories(temporary.path(), &["victim", "victim/nested"]);
        std::fs::create_dir(temporary.path().join("VICTIM.DELETED"))
            .expect("case-equivalent collision");
        std::fs::write(
            temporary.path().join("VICTIM.DELETED/replacement.bin"),
            b"replacement",
        )
        .expect("collision payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let directory = root
            .open_directory(&LeafName::new("victim").expect("victim leaf"))
            .expect("victim capability");
        let park_name = LeafName::new("victim.deleted").expect("park leaf");
        assert!(leaf_names_equivalent(
            park_name.as_os_str(),
            OsStr::new("VICTIM.DELETED"),
        ));

        let directory = match directory.park_as(park_name) {
            DirectoryParkOutcome::NoEffect { directory, .. } => directory,
            outcome => panic!("case-equivalent destination was not preserved: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(temporary.path().join("VICTIM.DELETED/replacement.bin"))
                .expect("collision payload"),
            b"replacement",
        );
        directory.entries(1).expect("source directory remains admitted");

        drop((directory, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn parked_tree_removal_unlinks_links_without_following_them() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root");
        let external = tempfile::tempdir().expect("external directory");
        std::fs::write(external.path().join("sentinel.bin"), b"external")
            .expect("external sentinel");
        create_test_directories(temporary.path(), &["victim", "victim/nested"]);
        symlink(external.path(), temporary.path().join("victim/nested/external"))
            .expect("external directory link");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "victim", "victim.deleted");

        assert!(matches!(
            parked.remove_tree(),
            DirectoryTreeRemovalOutcome::Removed
        ));
        assert_eq!(
            std::fs::read(external.path().join("sentinel.bin")).expect("external sentinel"),
            b"external",
        );

        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn parked_tree_removal_preserves_a_replacement_root_binding() {
        let temporary = tempfile::tempdir().expect("temporary root");
        create_test_directories(temporary.path(), &["victim", "victim/nested"]);
        std::fs::write(temporary.path().join("victim/nested/original.bin"), b"original")
            .expect("original payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "victim", "victim.deleted");
        std::fs::rename(
            temporary.path().join("victim.deleted"),
            temporary.path().join("victim.moved"),
        )
        .expect("move parked tree out of its binding");
        std::fs::create_dir(temporary.path().join("victim.deleted"))
            .expect("replacement root");
        std::fs::write(
            temporary.path().join("victim.deleted/replacement.bin"),
            b"replacement",
        )
        .expect("replacement payload");

        let obligation = match parked.remove_tree() {
            DirectoryTreeRemovalOutcome::Indeterminate(obligation) => obligation,
            outcome => panic!("replacement root was not retained as uncertain: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(temporary.path().join("victim.deleted/replacement.bin"))
                .expect("replacement payload"),
            b"replacement",
        );
        assert_eq!(
            std::fs::read(temporary.path().join("victim.moved/nested/original.bin"))
                .expect("original payload"),
            b"original",
        );

        std::fs::remove_file(temporary.path().join("victim.deleted/replacement.bin"))
            .expect("remove replacement payload");
        std::fs::remove_dir(temporary.path().join("victim.deleted"))
            .expect("remove replacement root");
        std::fs::rename(
            temporary.path().join("victim.moved"),
            temporary.path().join("victim.deleted"),
        )
        .expect("restore parked tree binding");
        assert!(matches!(
            obligation.reconcile(),
            DirectoryTreeRemovalResolution::Removed
        ));
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn bounded_tree_removal_can_be_retained_and_retried() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let mut nested = temporary.path().join("victim");
        std::fs::create_dir(&nested).expect("tree root");
        for _ in 0..=platform::MAX_TREE_CLEAR_DEPTH {
            nested.push("d");
            std::fs::create_dir(&nested).expect("nested bounded directory");
        }
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let owner = root.create_effect_owner().expect("effect owner");
        let parked = park_test_directory(&root, "victim", "victim.deleted");
        let obligation = match parked.remove_tree() {
            DirectoryTreeRemovalOutcome::Indeterminate(obligation) => obligation,
            outcome => panic!("over-depth tree was not retained as uncertain: {outcome:?}"),
        };
        assert!(temporary.path().join("victim.deleted").exists());
        owner
            .retain_directory_tree_removal(obligation)
            .expect("retain bounded tree obligation");

        drop((owner, root));
        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("unresolved tree removal did not refuse drain: {outcome:?}"),
        };
        assert!(temporary.path().join("victim.deleted").exists());

        let deepest = (0..=platform::MAX_TREE_CLEAR_DEPTH).fold(
            temporary.path().join("victim.deleted"),
            |path, _| path.join("d"),
        );
        std::fs::remove_dir(&deepest).expect("trim over-depth tree");
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
        assert!(!temporary.path().join("victim.deleted").exists());
    }

    #[test]
    fn raw_applied_directory_create_refuses_drain_until_reconciled() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let authority = session.authority.clone();
        let name = LeafName::new("created").expect("created leaf");
        let operation = authority.enter().expect("directory create operation");
        let token = authority
            .reserve_directory_create(&operation, &root, &name)
            .expect("directory create reservation");
        let created = match platform::create_directory(&root.inner.handle, name.as_os_str()) {
            Ok(created) => created,
            Err(_) => panic!("native directory creation failed"),
        };
        authority.attach_directory_create(&token, created);
        let obligation = DirectoryCreateObligation {
            error: io::Error::other("test directory create requires settlement"),
            token,
        };
        drop((operation, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("raw directory create did not refuse revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        let directory = match obligation.reconcile() {
            DirectoryCreateResolution::Created(directory) => directory,
            DirectoryCreateResolution::Indeterminate(_) => {
                panic!("raw directory create remained indeterminate")
            }
        };
        drop((directory, authority));
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn raw_live_stage_refuses_drain_and_remains_discardable() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let mut staged = match root.create_stage() {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => panic!("stage creation failed: {error}"),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                panic!("stage creation was not verified: {}", obligation.error())
            }
        };
        staged.write_all(b"pending").expect("stage bytes");
        drop(root);

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("raw live stage did not refuse revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn dropped_owner_settles_file_and_directory_restore_and_preservation_at_drain() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("restore.bin"), b"restore")
            .expect("restore file");
        std::fs::write(temporary.path().join("preserve.bin"), b"preserve")
            .expect("preserve file");
        std::fs::create_dir(temporary.path().join("folder")).expect("restore directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let owner = root.create_effect_owner().expect("effect owner");
        let restore_file = park_test_file(
            &root,
            "restore.bin",
            "restore.parked",
            b"restore",
        );
        owner
            .retain_parked_file_restore(restore_file)
            .expect("retain file restore");
        let preserve_file = park_test_file(
            &root,
            "preserve.bin",
            "preserve.parked",
            b"preserve",
        );
        owner
            .retain_parked_file_preservation(preserve_file)
            .expect("retain file preservation");
        let restore_directory = park_test_directory(&root, "folder", "folder.parked");
        owner
            .retain_parked_directory_restore(restore_directory)
            .expect("retain directory restore");

        let authority = session.authority.clone();
        let name = LeafName::new("preserved-directory").expect("preserved leaf");
        let operation = authority.enter().expect("directory create operation");
        let token = authority
            .reserve_directory_create(&operation, &root, &name)
            .expect("directory create reservation");
        let created = match platform::create_directory(&root.inner.handle, name.as_os_str()) {
            Ok(created) => created,
            Err(_) => panic!("native directory creation failed"),
        };
        authority.attach_directory_create(&token, created);
        authority.mark_directory_create_unclassified(&token);
        owner
            .retain_directory_create_preservation(DirectoryCreatePreservation { token })
            .expect("retain directory preservation");
        drop((operation, authority, owner, root));

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        assert_eq!(
            std::fs::read(temporary.path().join("restore.bin")).expect("restored file"),
            b"restore",
        );
        assert_eq!(
            std::fs::read(temporary.path().join("preserve.parked"))
                .expect("preserved file"),
            b"preserve",
        );
        assert!(temporary.path().join("folder").is_dir());
        assert!(temporary.path().join("preserved-directory").is_dir());
    }

    #[test]
    fn effect_owner_settlement_is_fifo_and_stops_at_first_unresolved_move() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/first.bin"), b"first")
            .expect("first file");
        std::fs::write(temporary.path().join("domain/second.bin"), b"second")
            .expect("second file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let first_file = domain
            .open_file(&LeafName::new("first.bin").expect("first leaf"))
            .expect("first capability");
        let second_file = domain
            .open_file(&LeafName::new("second.bin").expect("second leaf"))
            .expect("second capability");
        let first = owner
            .retain_file_move(test_file_move_obligation(
                first_file,
                domain.clone(),
                "first-target.bin",
                true,
            ))
            .expect("retain first move");
        let second = owner
            .retain_file_move(test_file_move_obligation(
                second_file,
                domain.clone(),
                "second-target.bin",
                false,
            ))
            .expect("retain second move");

        owner.settle().expect("first FIFO settlement");
        {
            let records = owner.state.effects.lock().expect("owner records");
            assert!(matches!(
                records.effects.get(&first.id),
                Some(OwnedEffect::FileMove(OwnedFileMove::Pending { .. }))
            ));
            assert!(matches!(
                records.effects.get(&second.id),
                Some(OwnedEffect::FileMove(OwnedFileMove::Pending { .. }))
            ));
        }
        {
            let mut records = owner.state.effects.lock().expect("owner records");
            let Some(OwnedEffect::FileMove(OwnedFileMove::Pending {
                obligation,
                ..
            })) = records.effects.get_mut(&first.id)
            else {
                panic!("first move remains pending")
            };
            obligation.reported_success = false;
        }
        owner.settle().expect("second FIFO settlement");
        let first_file = claim_no_effect(first);
        let second_file = claim_no_effect(second);
        assert!(owner.require_settled().is_ok());

        drop((first_file, second_file, owner, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn promotion_replace_and_directory_move_receipts_preserve_linear_outcomes() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("movable")).expect("movable directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let owner = root.create_effect_owner().expect("effect owner");

        let promoted_stage = test_sealed_stage(&root, b"promotion");
        let promotion = FilePromotionObligation {
            error: io::Error::other("test promotion requires settlement"),
            retained: promoted_stage,
            destination: root.clone(),
            destination_name: LeafName::new("promotion.bin").expect("promotion leaf"),
        };
        let promotion = owner
            .retain_file_promotion(promotion)
            .expect("retain promotion");
        owner.settle().expect("settle promotion");
        let staged = match promotion.claim() {
            FilePromotionReceiptOutcome::NoEffect(staged) => staged,
            FilePromotionReceiptOutcome::Applied(_) => {
                panic!("test promotion unexpectedly applied")
            }
            FilePromotionReceiptOutcome::Pending(_) => {
                panic!("test promotion remained pending")
            }
        };
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));

        let replacement_stage = test_sealed_stage(&root, b"replacement");
        let replacement_promotion = FilePromotionObligation {
            error: io::Error::other("test replacement promotion requires settlement"),
            retained: replacement_stage,
            destination: root.clone(),
            destination_name: LeafName::new("replacement.bin").expect("replacement leaf"),
        };
        let replacement = owner
            .retain_file_replace(FileReplaceObligation {
                error: io::Error::other("test replacement requires settlement"),
                state: Some(FileReplaceObligationState::Promoting {
                    promotion: replacement_promotion,
                    displaced: None,
                    fallback: ReplaceDestination::Vacant {
                        parent: root.clone(),
                        name: LeafName::new("replacement.bin").expect("replacement leaf"),
                    },
                    receipt: None,
                }),
            })
            .expect("retain replacement");
        drop(replacement);
        owner.settle().expect("reclaim dropped replacement result");

        let movable = root
            .open_directory(&LeafName::new("movable").expect("movable leaf"))
            .expect("movable capability");
        let directory_move = owner
            .retain_directory_move(test_directory_move_obligation(
                movable,
                root.clone(),
                "moved",
                false,
            ))
            .expect("retain directory move");
        owner.settle().expect("settle directory move");
        let movable = match directory_move.claim() {
            DirectoryMoveReceiptOutcome::NoEffect(directory) => directory,
            DirectoryMoveReceiptOutcome::Applied(_) => {
                panic!("test directory move unexpectedly applied")
            }
            DirectoryMoveReceiptOutcome::Pending(_) => {
                panic!("test directory move remained pending")
            }
        };
        drop((movable, owner, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn retained_replacement_restores_during_terminal_settlement() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("destination.bin"), b"old")
            .expect("destination file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let owner = root.create_effect_owner().expect("effect owner");
        let destination = root
            .open_file(&LeafName::new("destination.bin").expect("destination leaf"))
            .expect("destination capability");
        let revision = destination.revision().expect("destination revision");
        let digest: [u8; 32] = Sha256::digest(b"old").into();
        let request = destination.park_request(ExpectedFileContent::new(revision, digest));
        let expected = ExpectedContentReceipt::capture(&request);
        let parked = match root.park_file_as(
            request,
            LeafName::new("destination.parked").expect("park leaf"),
        ) {
            FileParkOutcome::Parked(parked) => parked,
            FileParkOutcome::NoEffect { error, .. } => {
                panic!("destination park had no effect: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                panic!("destination park was not verified: {}", obligation.error())
            }
        };
        let staged = test_sealed_stage(&root, b"new");
        let receipt = owner
            .retain_file_replace(FileReplaceObligation {
                error: io::Error::other("test replacement rollback requires settlement"),
                state: Some(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt: expected,
                }),
            })
            .expect("retain replacement rollback");
        drop((owner, root));

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live replacement receipt did not block revocation: {outcome:?}"),
        };
        let (staged, destination) = match receipt.claim() {
            FileReplaceReceiptOutcome::NoEffect {
                staged,
                destination,
            } => (staged, destination),
            FileReplaceReceiptOutcome::Replaced { .. } => {
                panic!("test replacement unexpectedly applied")
            }
            FileReplaceReceiptOutcome::Pending(_) => {
                panic!("test replacement remained pending")
            }
        };
        assert!(matches!(&destination, ReplaceDestination::Existing(_)));
        drop(destination);
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
        assert_eq!(
            std::fs::read(temporary.path().join("destination.bin"))
                .expect("restored destination"),
            b"old",
        );
    }

    #[test]
    fn effect_owner_counts_are_bounded_and_dead_handles_release_capacity() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let mut owners = (0..MAX_EFFECT_OWNERS)
            .map(|_| domain.create_effect_owner().expect("bounded effect owner"))
            .collect::<Vec<_>>();
        let error = domain
            .create_effect_owner()
            .expect_err("owner capacity must apply backpressure");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        owners.pop();
        owners.push(
            domain
                .create_effect_owner()
                .expect("dead owner handle releases capacity"),
        );

        drop((owners, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn effect_and_terminal_result_counts_share_one_bounded_capacity() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/source.bin"), b"source")
            .expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let mut receipts = Vec::with_capacity(MAX_EFFECTS_PER_OWNER);
        for index in 0..MAX_EFFECTS_PER_OWNER {
            let file = domain
                .open_file(&LeafName::new("source.bin").expect("source leaf"))
                .expect("source capability");
            receipts.push(
                owner
                    .retain_file_move(test_file_move_obligation(
                        file,
                        domain.clone(),
                        &format!("target-{index}.bin"),
                        false,
                    ))
                    .expect("bounded retained move"),
            );
        }
        let overflow_file = domain
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("overflow source capability");
        let failure = owner
            .retain_file_move(test_file_move_obligation(
                overflow_file,
                domain.clone(),
                "overflow-target.bin",
                false,
            ))
            .expect_err("effect capacity must apply backpressure");
        assert_eq!(failure.error().kind(), io::ErrorKind::WouldBlock);
        let (_, mut overflow) = failure.into_parts();
        let overflow_file = overflow.file.take().expect("returned overflow file");
        let authority = overflow_file.parent.authority().expect("overflow authority");
        let operation = authority.enter().expect("overflow settlement operation");
        overflow
            .token
            .settle(&operation)
            .expect("settle returned overflow token");
        drop((operation, overflow_file, overflow));

        owner.settle().expect("settle bounded effects");
        assert!(owner.require_settled().is_err());
        for receipt in receipts {
            drop(claim_no_effect(receipt));
        }
        assert!(owner.require_settled().is_ok());

        drop((owner, domain, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn leaf_names_are_exact_single_components() {
        for value in ["", ".", "..", "nested/name"] {
            assert!(LeafName::new(value).is_err());
        }
        assert!(LeafName::new("state.json").is_ok());
    }

    #[test]
    fn a_second_root_session_fails_without_waiting() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        drop(first);
        drop(acquire_test_root(temporary.path()));
    }

    #[test]
    fn file_capabilities_compare_private_physical_identity() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("first.bin"), b"first").expect("first file");
        std::fs::write(temporary.path().join("second.bin"), b"second").expect("second file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let first = root
            .open_file(&LeafName::new("first.bin").expect("first leaf"))
            .expect("first capability");
        let first_again = root
            .open_file(&LeafName::new("first.bin").expect("first leaf"))
            .expect("second first capability");
        let second = root
            .open_file(&LeafName::new("second.bin").expect("second leaf"))
            .expect("second capability");

        assert!(first.same_file(&first_again).expect("same-file proof"));
        assert!(!first.same_file(&second).expect("distinct-file proof"));
        drop((root, first, first_again, second));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn move_topology_never_reclassifies_reported_success_as_no_effect() {
        assert_eq!(
            classify_move_topology(
                true,
                Some(platform::BindingState::Exact),
                Some(platform::BindingState::Absent),
            ),
            MoveTopology::Indeterminate,
        );
        assert_eq!(
            classify_move_topology(
                false,
                Some(platform::BindingState::Exact),
                Some(platform::BindingState::Absent),
            ),
            MoveTopology::NoEffect,
        );
        assert_eq!(
            classify_move_topology(
                true,
                Some(platform::BindingState::Absent),
                Some(platform::BindingState::Exact),
            ),
            MoveTopology::Applied,
        );
    }

    #[test]
    fn unsettled_move_is_valid_pending_state_and_settlement_restores_drainability() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let authority = session.authority.clone();
        let mut token = {
            let operation = authority.enter().expect("move reservation operation");
            MoveEffectToken::reserve(
                &authority,
                &operation,
                NamespaceLeaf {
                    parent: session.root().expect("source parent"),
                    name: LeafName::new("source.bin").expect("source leaf"),
                },
                NamespaceLeaf {
                    parent: session.root().expect("destination parent"),
                    name: LeafName::new("destination.bin").expect("destination leaf"),
                },
                None,
            )
            .expect("move effect reservation")
        };
        {
            let state = authority.operations.lock().expect("operation state");
            assert_eq!(state.outstanding_effects, 1);
            assert_eq!(state.moves.len(), 1);
            assert!(state.moves.contains_key(&token.id));
            assert_eq!(state.phase, AUTHORITY_LIVE);
        }
        let error = authority
            .begin_terminal_drain()
            .expect_err("unsettled move blocks terminal drain");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        assert_eq!(
            authority.operations.lock().expect("operation state").phase,
            AUTHORITY_LIVE,
        );
        {
            let operation = authority.enter().expect("move settlement operation");
            token.settle(&operation).expect("settle move effect");
        }
        {
            let state = authority.operations.lock().expect("operation state");
            assert_eq!(state.outstanding_effects, 0);
            assert!(state.moves.is_empty());
        }
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn file_move_is_no_replace_and_collision_is_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("source")).expect("source directory");
        std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
        std::fs::write(temporary.path().join("source/moved.bin"), b"moved")
            .expect("moved source");
        std::fs::write(temporary.path().join("source/collision.bin"), b"source")
            .expect("collision source");
        std::fs::write(temporary.path().join("destination/collision.bin"), b"destination")
            .expect("collision destination");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let source = root
            .open_directory(&LeafName::new("source").expect("source leaf"))
            .expect("source capability");
        let destination = root
            .open_directory(&LeafName::new("destination").expect("destination leaf"))
            .expect("destination capability");

        let moved = source
            .open_file(&LeafName::new("moved.bin").expect("moved leaf"))
            .expect("moved capability");
        let moved = match moved.move_no_replace(
            &destination,
            &LeafName::new("published.bin").expect("published leaf"),
        ) {
            FileMoveOutcome::Applied(file) => file,
            FileMoveOutcome::NoEffect { error, .. } => panic!("move had no effect: {error}"),
            FileMoveOutcome::AppliedUnverified(obligation) => {
                panic!("move was indeterminate: {}", obligation.error())
            }
        };
        assert_eq!(
            moved.read_bounded(16).expect("moved bytes"),
            b"moved"
        );

        let collision = source
            .open_file(&LeafName::new("collision.bin").expect("collision leaf"))
            .expect("collision source capability");
        match collision.move_no_replace(
            &destination,
            &LeafName::new("collision.bin").expect("collision leaf"),
        ) {
            FileMoveOutcome::NoEffect { file, .. } => {
                assert_eq!(file.read_bounded(16).expect("source bytes"), b"source");
            }
            FileMoveOutcome::Applied(_) => panic!("collision replaced its destination"),
            FileMoveOutcome::AppliedUnverified(obligation) => {
                panic!("collision was indeterminate: {}", obligation.error())
            }
        }
        assert_eq!(
            std::fs::read(temporary.path().join("destination/collision.bin"))
                .expect("destination bytes"),
            b"destination"
        );
        drop((root, source, destination, moved));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn directory_move_is_no_replace_and_collision_is_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary root");
        for relative in [
            "source",
            "destination",
            "source/moved",
            "source/collision",
            "destination/collision",
        ] {
            std::fs::create_dir(temporary.path().join(relative)).expect("test directory");
        }
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let source = root
            .open_directory(&LeafName::new("source").expect("source leaf"))
            .expect("source capability");
        let destination = root
            .open_directory(&LeafName::new("destination").expect("destination leaf"))
            .expect("destination capability");

        let moved = source
            .open_directory(&LeafName::new("moved").expect("moved leaf"))
            .expect("moved capability");
        let moved = match moved.move_no_replace(
            &destination,
            &LeafName::new("published").expect("published leaf"),
        ) {
            DirectoryMoveOutcome::Applied(directory) => directory,
            DirectoryMoveOutcome::NoEffect { error, .. } => {
                panic!("directory move had no effect: {error}")
            }
            DirectoryMoveOutcome::AppliedUnverified(obligation) => {
                panic!("directory move was indeterminate: {}", obligation.error())
            }
        };
        moved.entries(1).expect("moved directory remains admitted");

        let collision = source
            .open_directory(&LeafName::new("collision").expect("collision leaf"))
            .expect("collision source capability");
        match collision.move_no_replace(
            &destination,
            &LeafName::new("collision").expect("collision leaf"),
        ) {
            DirectoryMoveOutcome::NoEffect { directory, .. } => {
                directory.entries(1).expect("source remains admitted");
            }
            DirectoryMoveOutcome::Applied(_) => panic!("collision replaced its destination"),
            DirectoryMoveOutcome::AppliedUnverified(obligation) => {
                panic!("collision was indeterminate: {}", obligation.error())
            }
        }
        assert!(temporary.path().join("destination/collision").is_dir());
        drop((root, source, destination, moved));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn clear_receipt_retains_the_root_lease_until_release() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("owned.bin"), b"owned").expect("owned file");
        let session = acquire_test_root(temporary.path());
        let reset = match session.begin_reset() {
            ResetStartOutcome::Ready(authority) => authority,
            outcome => panic!("reset did not become ready: {outcome:?}"),
        };
        let receipt = match reset.clear_root() {
            RootClearOutcome::Cleared(receipt) => receipt,
            RootClearOutcome::Failed(failure) => {
                panic!("root clear failed: {}", failure.error())
            }
        };
        assert!(!temporary.path().join("owned.bin").exists());
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        receipt.release().expect("release clear receipt");
        drop(acquire_test_root(temporary.path()));
    }

    #[test]
    fn absolute_directory_containment_uses_retained_physical_ancestry() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let nested = temporary.path().join("user-library");
        std::fs::create_dir(&nested).expect("nested directory");
        let external = tempfile::tempdir().expect("external directory");
        let session = acquire_test_root(temporary.path());

        assert!(matches!(
            session.admit_absolute_directory_authority_outside_root(temporary.path()),
            AbsoluteDirectoryOutsideRootAdmission::InsideRoot
        ));
        assert_eq!(
            session
                .validate_absolute_directory_outside_root(temporary.path())
                .expect_err("root itself is not external")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            session
                .validate_absolute_directory_outside_root(&nested)
                .expect_err("nested directory is not external")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        session
            .validate_absolute_directory_outside_root(external.path())
            .expect("sibling physical directory is external");

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_directory_containment_rejects_symlink_ancestry() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root");
        let external = tempfile::tempdir().expect("external directory");
        let alias = temporary.path().join("external-alias");
        symlink(external.path(), &alias).expect("external alias");
        let session = acquire_test_root(temporary.path());

        session
            .validate_absolute_directory_outside_root(&alias)
            .expect_err("symlink ancestry must fail closed");

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revoked_capabilities_refuse_operations() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        assert_eq!(
            root.entries(1).expect_err("revoked capability must refuse").kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[test]
    fn leaf_name_equivalence_covers_case_folding_and_normalization() {
        let equivalent = |first: &OsStr, second: &OsStr| {
            assert!(platform::leaf_names_equal(first, second));
            let first_keys = leaf_name_equivalence_keys(first);
            let second_keys = leaf_name_equivalence_keys(second);
            assert!(first_keys.iter().any(|key| second_keys.contains(key)));
        };
        equivalent(OsStr::new("state.json"), OsStr::new("STATE.JSON"));
        equivalent(OsStr::new("Stra\u{00df}e"), OsStr::new("STRASSE"));
        equivalent(OsStr::new("\u{00e9}"), OsStr::new("E\u{0301}"));
        assert!(!platform::leaf_names_equal(
            OsStr::new("state.json"),
            OsStr::new("other.json"),
        ));
    }

    #[cfg(unix)]
    #[test]
    fn non_utf8_leaf_keys_preserve_exact_native_identity() {
        use std::os::unix::ffi::OsStrExt;

        let first = OsStr::from_bytes(b"state-\xff.json");
        let same = OsStr::from_bytes(b"state-\xff.json");
        let different = OsStr::from_bytes(b"state-\xfe.json");
        let first_keys = leaf_name_equivalence_keys(first);
        assert!(first_keys
            .iter()
            .any(|key| leaf_name_equivalence_keys(same).contains(key)));
        assert!(!first_keys
            .iter()
            .any(|key| leaf_name_equivalence_keys(different).contains(key)));
    }

    #[test]
    fn named_parks_reject_the_same_native_binding_without_registry_effects() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("World")).expect("test directory");
        std::fs::write(temporary.path().join("State.bin"), b"state").expect("test file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");

        let directory = root
            .open_directory(&LeafName::new("World").expect("directory leaf"))
            .expect("directory capability");
        let directory_error = match directory
            .park_as(LeafName::new("WORLD").expect("directory alias"))
        {
            DirectoryParkOutcome::NoEffect { error, directory } => {
                drop(directory);
                error
            }
            DirectoryParkOutcome::Parked(_) => panic!("same-binding directory park applied"),
            DirectoryParkOutcome::AppliedUnverified(_) => {
                panic!("same-binding directory park became indeterminate")
            }
        };

        let file = root
            .open_file(&LeafName::new("State.bin").expect("file leaf"))
            .expect("file capability");
        let revision = file.revision().expect("file revision");
        let request = file.park_request(ExpectedFileContent::new(revision, [0_u8; 32]));
        let file_error = match root
            .park_file_as(request, LeafName::new("STATE.BIN").expect("file alias"))
        {
            FileParkOutcome::NoEffect { error, request } => {
                drop(request);
                error
            }
            FileParkOutcome::Parked(_) => panic!("same-binding file park applied"),
            FileParkOutcome::AppliedUnverified(_) => {
                panic!("same-binding file park became indeterminate")
            }
        };

        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 0);
        assert!(state.file_parks.is_empty());
        assert!(state.directory_parks.is_empty());
        assert!(state.park_owners.is_empty());
        drop(state);
        assert_eq!(directory_error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(file_error.kind(), io::ErrorKind::InvalidInput);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn preserved_file_acknowledgement_leaves_the_leaf_and_clears_ownership() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_preservation_test_file(&root);

        parked
            .validate_current()
            .expect("validate preserved file");
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("operation state");
            assert_eq!(state.outstanding_effects, 1);
            assert_eq!(state.file_parks_checked_out, 0);
            assert_eq!(state.file_parks.len(), 1);
            assert_eq!(state.park_owners.len(), 1);
        }

        parked
            .acknowledge_preserved()
            .expect("acknowledge preserved file");

        assert!(!temporary.path().join("record.bin").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("record.preserved"))
                .expect("read preserved file"),
            b"payload",
        );
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 0);
        assert_eq!(state.file_parks_checked_out, 0);
        assert!(state.file_parks.is_empty());
        assert!(state.park_owners.is_empty());
        drop(state);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn preserved_file_acknowledgement_reinserts_mismatched_park_ownership() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_preservation_test_file(&root);
        {
            let mut state = session
                .authority
                .operations
                .lock()
                .expect("operation state");
            let record = state
                .file_parks
                .get_mut(&parked.token.id)
                .expect("registered park");
            record.size = record.size.checked_add(1).expect("mismatched park size");
        }

        assert_eq!(
            parked
                .validate_current()
                .expect_err("mismatched park must fail current validation")
                .kind(),
            io::ErrorKind::InvalidData,
        );

        let error = parked
            .acknowledge_preserved()
            .expect_err("mismatched park must retain ownership");
        assert_eq!(error.error().kind(), io::ErrorKind::InvalidData);
        let parked = error.into_parked();
        assert!(parked.token.armed);
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 1);
        assert_eq!(state.file_parks_checked_out, 0);
        assert_eq!(state.file_parks.len(), 1);
        assert_eq!(state.park_owners.len(), 1);
        drop(state);

        discard_test_park_registration(parked);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn preserved_file_acknowledgement_returns_mutated_park_ownership() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_preservation_test_file(&root);
        std::fs::write(
            temporary.path().join("record.preserved"),
            b"mutated-payload",
        )
        .expect("mutate preserved file");

        let error = parked
            .acknowledge_preserved()
            .expect_err("mutated park must retain ownership");
        assert_eq!(error.error().kind(), io::ErrorKind::InvalidData);
        let parked = error.into_parked();
        assert!(parked.token.armed);
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 1);
        assert_eq!(state.file_parks_checked_out, 0);
        assert_eq!(state.file_parks.len(), 1);
        assert_eq!(state.park_owners.len(), 1);
        drop(state);

        discard_test_park_registration(parked);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revisions_and_bounded_ranges_are_exact() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("first")).expect("first directory");
        std::fs::create_dir(temporary.path().join("second")).expect("second directory");
        std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let first = root
            .open_directory(&LeafName::new("first").expect("first leaf"))
            .expect("first capability");
        let second = root
            .open_directory(&LeafName::new("second").expect("second leaf"))
            .expect("second capability");
        let directory_revision = first.revision().expect("directory revision");
        first
            .validate_revision(&directory_revision)
            .expect("same directory revision");
        assert_eq!(
            second
                .validate_revision(&directory_revision)
                .expect_err("another directory cannot match the revision")
                .kind(),
            io::ErrorKind::InvalidData,
        );

        let file = root
            .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
            .expect("file capability");
        let revision = file.revision().expect("file revision");
        assert_eq!(revision.size(), 6);
        let _ = revision.modified_at_ns().expect("mtime");
        let _ = revision.changed_at_ns().expect("ctime");
        assert_eq!(
            file.read_range_bounded(&revision, 2, 3)
                .expect("bounded range"),
            b"cde",
        );
        assert!(
            file.read_range_bounded(&revision, 6, 0)
                .expect("empty terminal range")
                .is_empty()
        );
        assert_eq!(
            file.read_range_bounded(&revision, 0, MAX_FILE_RANGE_BYTES + 1)
                .expect_err("oversized range")
                .kind(),
            io::ErrorKind::InvalidInput,
        );
        assert_eq!(
            file.read_range_bounded(&revision, 5, 2)
                .expect_err("range beyond revision")
                .kind(),
            io::ErrorKind::UnexpectedEof,
        );
        assert_eq!(
            file.read_range_bounded(&revision, u64::MAX, 1)
                .expect_err("overflowing range")
                .kind(),
            io::ErrorKind::InvalidInput,
        );
        let mut reader = file
            .into_revision_reader(revision, 6)
            .expect("owned revision reader");
        let mut first = [0_u8; 2];
        reader.read_exact(&mut first).expect("initial read");
        assert_eq!(&first, b"ab");
        assert_eq!(reader.seek(SeekFrom::End(-3)).expect("tail seek"), 3);
        let mut tail = Vec::new();
        reader.read_to_end(&mut tail).expect("tail read");
        assert_eq!(tail, b"def");
        assert_eq!(
            reader
                .seek(SeekFrom::Current(1))
                .expect_err("seek beyond revision")
                .kind(),
            io::ErrorKind::InvalidInput,
        );
        let file = reader.finish().expect("stable reader finish");
        drop(file);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revision_reader_start_failures_retain_every_input() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
        std::fs::write(temporary.path().join("other.bin"), b"other").expect("other file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");

        let sample_name = LeafName::new("sample.bin").expect("sample leaf");
        let file = root.open_file(&sample_name).expect("sample capability");
        let revision = file.revision().expect("sample revision");
        let failure = file
            .into_revision_reader(revision, 5)
            .expect_err("reader bound must reject the revision");
        assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
        let (error, file, revision, max_bytes) = failure.into_parts();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(max_bytes, 5);
        let reader = file
            .into_revision_reader(revision, 6)
            .expect("retained inputs can start a corrected reader");
        let (file, revision) = reader.cancel();
        file.validate_revision(&revision)
            .expect("cancel returns the original capability and revision without proof");
        drop((file, revision));

        let file = root.open_file(&sample_name).expect("sample capability");
        let other = root
            .open_file(&LeafName::new("other.bin").expect("other leaf"))
            .expect("other capability");
        let other_revision = other.revision().expect("other revision");
        let failure = file
            .into_revision_reader(other_revision, 16)
            .expect_err("foreign revision must be rejected");
        assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
        let failure = failure.retry().expect_err("foreign revision remains foreign");
        let (_, file, other_revision, max_bytes) = failure.into_parts();
        assert_eq!(max_bytes, 16);
        drop((file, other, other_revision, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revision_reader_operation_blocks_revocation_until_finish() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let file = root
            .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
            .expect("sample capability");
        let revision = file.revision().expect("sample revision");
        let reader = file
            .into_revision_reader(revision, 6)
            .expect("owned revision reader");

        let refusal = match session.revoke() {
            RootRevokeOutcome::Refused(failure) => failure,
            outcome => panic!("live reader operation did not block revocation: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        let file = reader.finish().expect("stable reader finish");
        drop((file, root));
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revision_reader_operation_blocks_reset_until_cancel() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("sample.bin"), b"abcdef").expect("sample file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let file = root
            .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
            .expect("sample capability");
        let revision = file.revision().expect("sample revision");
        let reader = file
            .into_revision_reader(revision, 6)
            .expect("owned revision reader");

        let refusal = match session.begin_reset() {
            ResetStartOutcome::Refused(failure) => failure,
            outcome => panic!("live reader operation did not block reset: {outcome:?}"),
        };
        assert_eq!(refusal.error().kind(), io::ErrorKind::WouldBlock);
        let (file, revision) = reader.cancel();
        let session = refusal.cancel_reset();
        drop((file, revision, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn revision_reader_finish_failure_retains_the_reader() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let path = temporary.path().join("sample.bin");
        std::fs::write(&path, b"abcdef").expect("sample file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let file = root
            .open_file(&LeafName::new("sample.bin").expect("sample leaf"))
            .expect("sample capability");
        let revision = file.revision().expect("sample revision");
        let reader = file
            .into_revision_reader(revision, 6)
            .expect("owned revision reader");
        std::fs::write(&path, b"changed").expect("mutate admitted file");

        let failure = reader
            .finish()
            .expect_err("changed revision must fail final settlement");
        assert_eq!(failure.error().kind(), io::ErrorKind::InvalidData);
        let reader = failure.into_reader();
        let (file, revision) = reader.cancel();
        assert_eq!(
            file.validate_revision(&revision)
                .expect_err("cancel does not claim a stable revision")
                .kind(),
            io::ErrorKind::InvalidData,
        );
        drop((file, revision, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn shared_park_ownership_survives_file_record_checkout() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("file.parked"), b"payload")
            .expect("parked file");
        std::fs::create_dir(temporary.path().join("directory.parked"))
            .expect("parked directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");

        let file = root
            .open_file(&LeafName::new("file.parked").expect("file park leaf"))
            .expect("parked file capability");
        let revision = file.revision().expect("file revision");
        let digest: [u8; 32] = Sha256::digest(b"payload").into();
        let request = file.park_request(ExpectedFileContent::new(revision, digest));
        let mut parked_file = root
            .admit_existing_file_park(
                &LeafName::new("file.original").expect("file original leaf"),
                request,
            )
            .expect("existing file park admission");

        let directory = root
            .open_directory(
                &LeafName::new("directory.parked").expect("directory park leaf"),
            )
            .expect("parked directory capability");
        let directory_revision = directory.revision().expect("directory revision");
        let conflicting_key = ParkRegistryKey::new(
            &root,
            &LeafName::new("FILE.ORIGINAL").expect("aliasing original leaf"),
            &LeafName::new("directory.parked").expect("directory park leaf"),
            directory.inner.identity.physical,
        );

        let authority = parked_file.authority().expect("park authority");
        let operation = authority
            .enter_file_park(&parked_file.token)
            .expect("file park operation");
        let guard = authority
            .take_file_park(&operation, &parked_file.token)
            .expect("checked-out file park");
        let checked_out_conflict = authority
            .ensure_park_available(&operation, &conflicting_key)
            .map_err(|error| error.kind());
        drop(guard);
        drop(operation);

        let admission_error = root
            .admit_existing_directory_park(
                &LeafName::new("FILE.ORIGINAL").expect("aliasing original leaf"),
                directory,
                &directory_revision,
            )
            .expect_err("cross-kind alias must remain owned")
            .kind();

        let operation = authority
            .enter_file_park(&parked_file.token)
            .expect("file park cleanup operation");
        let guard = authority
            .take_file_park(&operation, &parked_file.token)
            .expect("file park cleanup guard");
        guard.disarm(&mut parked_file.token, &operation);
        drop(operation);
        drop(parked_file);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));

        assert_eq!(checked_out_conflict, Err(io::ErrorKind::AlreadyExists));
        assert_eq!(admission_error, io::ErrorKind::AlreadyExists);
    }
}
