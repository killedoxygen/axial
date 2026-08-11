mod control_frame;
mod platform;
mod recovery;
mod recovery_runtime;
mod successor;
mod transient;

pub use transient::{
    TransientCreationObligation, TransientDestination, TransientDestinationBatch,
    TransientDestinationCancelObligation, TransientDestinationCancelOutcome,
    TransientDiscardObligation, TransientDiscardOutcome, TransientPublicationBatch,
    TransientPublicationBatchCreateFailure, TransientPublicationBatchObligation,
    TransientPublicationBatchOutcome, TransientPublicationMember, TransientStage,
    TransientStageCreateOutcome, TransientStageSealFailure, TransientStageSealed,
};

use std::borrow::Borrow;
use std::cell::Cell;
use std::collections::{BTreeMap, HashMap};
use std::ffi::{OsStr, OsString};
use std::fmt;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
#[cfg(unix)]
use std::path::Component;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};

use recovery::{
    RecoveryJournal, RecoveryName, RecoveryPhase, RecoveryRecord, RecoveryRegistration,
    StateSuccessorDescriptor, SuccessorOwner, recovery_park_leaf, recovery_stage_leaf,
};

struct LiveStateCarrier {
    registration: RecoveryRegistration,
    handle: File,
    identity: platform::Identity,
    receipt: (u64, platform::FileStamp),
    proof: recovery::RecoveryFileProof,
}

fn recovery_owns_park(record: &RecoveryRecord) -> bool {
    record.old.is_some()
}

const ROOT_LEASE_NAME: &str = ".axial-root.lease";
const MAX_LEAF_UNITS: usize = 255;
const MAX_STAGE_ATTEMPTS: usize = 32;
pub const MAX_DIRECTORY_LIST_ENTRIES: usize = 100_000;
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

#[cfg(unix)]
pub fn resolve_owned_symlink_target_beneath(
    parent: &[LeafName],
    target: &OsStr,
) -> io::Result<Vec<LeafName>> {
    let path = Path::new(target);
    if target.is_empty() || path.is_absolute() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owned symlink target must be a nonempty relative path",
        ));
    }
    let mut resolved = parent.to_vec();
    for component in path.components() {
        match component {
            Component::Normal(name) => {
                resolved.push(LeafName::new(name.to_os_string()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "owned symlink target contains an invalid name",
                    )
                })?);
            }
            Component::CurDir => {}
            Component::ParentDir => {
                if resolved.pop().is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "owned symlink target escapes its retained root",
                    ));
                }
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "owned symlink target is not relative",
                ));
            }
        }
    }
    if resolved.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "owned symlink target must name an entry beneath its root",
        ));
    }
    Ok(resolved)
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
        match authority.acknowledge_unclassified_directory_create(&operation, &mut self.token) {
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
        match authority.acknowledge_unclassified_directory_create(&operation, &mut self.token) {
            Ok(()) => Ok(()),
            Err(_) => Err(self),
        }
    }

    fn transfer_to_reset(mut self, permit: &DrainRecoveryPermit) -> Result<(), Self> {
        let authority = match self.token.authority.upgrade() {
            Some(authority) => authority,
            None => return Err(self),
        };
        match authority.transfer_unclassified_directory_create_to_reset(permit, &mut self.token) {
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
        match reconcile_recovery_stage_create(&authority, &mut self.token) {
            Ok(Some((parent, name))) => {
                return match execute_stage_create(
                    &parent, &name, &authority, &operation, self.token,
                ) {
                    FileCreateOutcome::Created(staged) => FileCreateResolution::Created(staged),
                    FileCreateOutcome::AppliedUnverified(obligation) => {
                        FileCreateResolution::Indeterminate(obligation)
                    }
                    FileCreateOutcome::NoEffect(error) => FileCreateResolution::NoEffect(error),
                };
            }
            Ok(None) => {}
            Err(_) => return FileCreateResolution::Indeterminate(self),
        }
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
        return Err(io::Error::other(
            "stage create effect is not yet classified",
        ));
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
        reservation,
        operation,
    )?;
    let handle = guard
        .record_mut()
        .created
        .take()
        .expect("classified stage create retains its handle");
    guard.finish_transfer(reservation);
    Ok(StagedFile {
        file: FileCapability::new(handle, identity, parent, name, Arc::downgrade(authority)),
        token: stage_token,
    })
}

fn execute_stage_create(
    directory: &Directory,
    name: &LeafName,
    authority: &Arc<CapabilityAuthority>,
    operation: &CapabilityOperation,
    mut reservation: StageCreateToken,
) -> FileCreateOutcome {
    let handle = match platform::create_file(&directory.inner.handle, name.as_os_str()) {
        Ok(handle) => handle,
        Err(platform::CreateFileError::NoEffect(error)) => {
            match authority.take_stage_create(operation, &reservation) {
                Ok(guard) => {
                    if let Err(settlement) = guard.disarm(&mut reservation, operation) {
                        return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                            error: io::Error::other(format!(
                                "stage create had no native effect but its recovery record did not settle: {error}; {settlement}"
                            )),
                            token: reservation,
                        });
                    }
                }
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
    match finish_stage_create(authority, operation, &mut reservation) {
        Ok(staged) => FileCreateOutcome::Created(staged),
        Err(error) => FileCreateOutcome::AppliedUnverified(FileCreateObligation {
            error,
            token: reservation,
        }),
    }
}

#[must_use = "file create resolutions must be handled"]
#[derive(Debug)]
pub enum FileCreateResolution {
    Created(StagedFile),
    NoEffect(io::Error),
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
    AppliedUnverified(Box<FilePromotionObligation>),
}

#[must_use = "file promotion obligations must be reconciled"]
pub struct FilePromotionObligation {
    error: io::Error,
    retained: SealedStagedFile,
    destination: Directory,
    destination_name: LeafName,
    attempt_id: u64,
    receipt: platform::PublicationReceipt,
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
            Err(_) => return FilePromotionResolution::Indeterminate(Box::new(self)),
        };
        let operation = match authority.enter() {
            Ok(operation) => operation,
            Err(_) => return FilePromotionResolution::Indeterminate(Box::new(self)),
        };
        if self.retained.file.parent.validate(&operation).is_err()
            || self.destination.validate(&operation).is_err()
            || platform::file_identity(&self.retained.file.handle).ok()
                != Some(self.retained.file.identity)
            || self
                .retained
                .file
                .validate_content_revision_in(&operation, &self.retained.revision)
                .is_err()
        {
            return FilePromotionResolution::Indeterminate(Box::new(self));
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
                if self
                    .retained
                    .token
                    .validate_publication_attempt(self.attempt_id, &self.receipt)
                    .is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                let settlement = platform::settle_publication(
                    &mut self.receipt,
                    self.attempt_id,
                    &self.retained.file.handle,
                    &self.retained.file.parent.inner.handle,
                    self.retained.file.name.as_os_str(),
                    &self.destination.inner.handle,
                    self.destination_name.as_os_str(),
                );
                self.retained
                    .token
                    .record_publication(self.attempt_id, self.receipt.clone());
                if let Err(error) = settlement {
                    self.error = error;
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                if self.destination.validate(&operation).is_err() {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                if let Err(error) = complete_recovery_publication(
                    &authority,
                    &operation,
                    &self.retained.token,
                    &self.retained.file,
                    &self.retained.revision,
                    &self.destination,
                    &self.destination_name,
                ) {
                    self.error = error;
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                let handle = match platform::open_file(
                    &self.destination.inner.handle,
                    self.destination_name.as_os_str(),
                ) {
                    Ok(handle)
                        if platform::file_identity(&handle).ok()
                            == Some(self.retained.file.identity) =>
                    {
                        handle
                    }
                    _ => return FilePromotionResolution::Indeterminate(Box::new(self)),
                };
                let applied = FileCapability::new(
                    handle,
                    self.retained.file.identity,
                    self.destination.clone(),
                    self.destination_name.clone(),
                    self.retained.file.authority.clone(),
                );
                if applied.validate(&operation).is_err()
                    || applied
                        .validate_content_revision_in(&operation, &self.retained.revision)
                        .is_err()
                    || self.retained.token.disarm().is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                FilePromotionResolution::Applied(applied)
            }
            (
                Ok(platform::BindingState::Exact),
                Ok(platform::BindingState::Absent | platform::BindingState::Occupied),
            ) if self.receipt.is_attempted() => {
                if self.retained.file.validate(&operation).is_err()
                    || self
                        .retained
                        .file
                        .validate_revision_in(&operation, &self.retained.revision)
                        .is_err()
                    || self
                        .retained
                        .token
                        .update(StageRegistryPhase::Sealed)
                        .is_err()
                {
                    return FilePromotionResolution::Indeterminate(Box::new(self));
                }
                FilePromotionResolution::NoEffect(self.retained)
            }
            _ => FilePromotionResolution::Indeterminate(Box::new(self)),
        }
    }
}

#[must_use = "file promotion resolutions must be handled"]
#[derive(Debug)]
pub enum FilePromotionResolution {
    Applied(FileCapability),
    NoEffect(SealedStagedFile),
    Indeterminate(Box<FilePromotionObligation>),
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
        let file = self
            .file
            .take()
            .expect("file move obligation retains its file");
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

#[must_use = "file-move park handoffs must be explicitly settled"]
#[derive(Debug)]
pub enum FileMoveAfterParkOutcome {
    Applied {
        current: FileCapability,
        displaced: ParkedFile,
    },
    NoEffect {
        error: io::Error,
        source: FileCapability,
        displaced: ParkedFile,
    },
    AppliedUnverified(FileMoveAfterParkObligation),
}

#[must_use = "file-move park handoff resolutions must be handled"]
#[derive(Debug)]
pub enum FileMoveAfterParkResolution {
    Applied {
        current: FileCapability,
        displaced: ParkedFile,
    },
    NoEffect {
        source: FileCapability,
        displaced: ParkedFile,
    },
    Indeterminate(FileMoveAfterParkObligation),
}

#[must_use = "file-move park handoff obligations must be reconciled"]
pub struct FileMoveAfterParkObligation {
    movement: FileMoveObligation,
    displaced: ParkedFile,
}

impl_redacted_debug!(FileMoveAfterParkObligation);

impl FileMoveAfterParkObligation {
    pub fn error(&self) -> &io::Error {
        self.movement.error()
    }

    pub fn reconcile(self) -> FileMoveAfterParkResolution {
        let Self {
            movement,
            displaced,
        } = self;
        match movement.reconcile() {
            FileMoveResolution::Applied(current) => {
                FileMoveAfterParkResolution::Applied { current, displaced }
            }
            FileMoveResolution::NoEffect(source) => {
                FileMoveAfterParkResolution::NoEffect { source, displaced }
            }
            FileMoveResolution::Indeterminate(movement) => {
                FileMoveAfterParkResolution::Indeterminate(Self {
                    movement,
                    displaced,
                })
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
    Vacant {
        parent: Directory,
        name: LeafName,
    },
    Existing(FileParkRequest),
    /// A changed destination was restored without granting deletion authority.
    /// Callers need a fresh content admission before another replacement attempt.
    Preserved(FileCapability),
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
    sha256: [u8; 32],
}

impl ExpectedContentReceipt {
    fn capture(request: &FileParkRequest) -> Self {
        Self {
            authority: request.expected.revision.authority.clone(),
            identity: request.expected.revision.identity,
            size: request.expected.revision.size,
            sha256: request.expected.sha256,
        }
    }

    fn rebuild_after_restore(self, file: FileCapability) -> ReplaceDestination {
        use sha2::Digest;

        let revision = match file.revision() {
            Ok(revision)
                if Weak::ptr_eq(&revision.authority, &self.authority)
                    && revision.identity == self.identity
                    && revision.size == self.size =>
            {
                revision
            }
            Ok(_) | Err(_) => return ReplaceDestination::Preserved(file),
        };
        let mut reader = match file.reader(self.size) {
            Ok(reader) => reader,
            Err(_) => return ReplaceDestination::Preserved(file),
        };
        let mut hasher = sha2::Sha256::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => hasher.update(&buffer[..read]),
                Err(_) => return ReplaceDestination::Preserved(file),
            }
        }
        if reader.finish().is_err()
            || file.validate_revision(&revision).is_err()
            || <[u8; 32]>::from(hasher.finalize()) != self.sha256
        {
            return ReplaceDestination::Preserved(file);
        }
        ReplaceDestination::Existing(
            file.park_request(ExpectedFileContent::new(revision, self.sha256)),
        )
    }
}

enum FileReplaceObligationState {
    Parking {
        park: FileParkObligation,
        staged: SealedStagedFile,
        receipt: ExpectedContentReceipt,
    },
    Promoting {
        promotion: Box<FilePromotionObligation>,
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
    state: Option<Box<FileReplaceObligationState>>,
}

impl_redacted_debug!(FileReplaceObligation);

impl FileReplaceObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> FileReplaceResolution {
        let state = *self.state.take().expect("replace obligation retains state");
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
    NoEffect {
        error: io::Error,
        request: FileParkRequest,
    },
    /// The original binding was restored with stable but different content.
    Preserved {
        error: io::Error,
        file: FileCapability,
    },
    AppliedUnverified(FileParkObligation),
}

#[must_use = "file park resolutions must be handled"]
#[derive(Debug)]
pub enum FileParkResolution {
    Parked(ParkedFile),
    NoEffect(FileParkRequest),
    /// The original binding was restored with stable but different content.
    Preserved {
        error: io::Error,
        file: FileCapability,
    },
    Indeterminate(FileParkObligation),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileParkPhase {
    Parking,
    RestoringRejectedReceipt,
}

enum RestoredFileProof {
    Current(FileRevision),
    Preserved,
}

#[cfg(test)]
struct RestoredFileProofPause {
    hashed: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
}

#[must_use = "file park obligations must be reconciled"]
pub struct FileParkObligation {
    error: io::Error,
    request: Option<FileParkRequest>,
    token: FileParkRegistryToken,
    park_name: LeafName,
    phase: FileParkPhase,
    digest_verified: bool,
    #[cfg(test)]
    restored_proof_pause: Option<RestoredFileProofPause>,
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
    NoEffect {
        error: io::Error,
        parked: ParkedFile,
    },
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
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked file");
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

    fn reconcile_with_recovery(mut self, permit: &DrainRecoveryPermit) -> FileRemovalResolution {
        let parked = self
            .parked
            .take()
            .expect("removal obligation retains parked file");
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
    NoEffect {
        error: io::Error,
        parked: ParkedFile,
    },
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
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked file");
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

    fn reconcile_with_recovery(mut self, permit: &DrainRecoveryPermit) -> FileRestoreResolution {
        let parked = self
            .parked
            .take()
            .expect("restore obligation retains parked file");
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
        self,
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

    #[expect(
        clippy::result_large_err,
        reason = "the 160-byte public failure is immediately unpacked to recover the park and is never stored"
    )]
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
            return Err(identity_changed(
                "unverified parked file has no exact receipt",
            ));
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
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return FileRemovalOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        FileRemovalOutcome::NoEffect {
                            error,
                            parked: self,
                        }
                    }
                    _ => FileRemovalOutcome::AppliedUnverified(FileRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    fn remove_with_recovery(self, permit: &DrainRecoveryPermit) -> FileRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
            return FileRemovalOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        FileRemovalOutcome::NoEffect {
                            error,
                            parked: self,
                        }
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
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_file_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        FileRestoreOutcome::NoEffect {
                            error,
                            parked: self,
                        }
                    }
                    _ => FileRestoreOutcome::AppliedUnverified(FileRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    fn restore_with_recovery(self, permit: &DrainRecoveryPermit) -> FileRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_file_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        self.restore_admitted(authority, operation)
    }

    fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> FileRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return FileRestoreOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_file_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return FileRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        FileRestoreOutcome::NoEffect {
                            error,
                            parked: self,
                        }
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
        record: &FileParkSettlementRecord,
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
    NoEffect {
        error: io::Error,
        directory: Directory,
    },
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
    NoEffect {
        error: io::Error,
        parked: ParkedDirectory,
    },
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
    NoEffect {
        error: io::Error,
        parked: ParkedDirectory,
    },
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
    #[error("application root recovery could not be initialized or replayed")]
    Recovery(#[source] io::Error),
}

#[must_use = "State successor admission must be validated and consumed by root recovery"]
pub struct RootStateSuccessor {
    descriptor: StateSuccessorDescriptor,
}

impl_redacted_debug!(RootStateSuccessor);

#[derive(Clone, Eq, PartialEq)]
pub struct StateFileSuccessorRequest {
    owner_schema: u16,
    owner_id: Vec<u8>,
}

impl_redacted_debug!(StateFileSuccessorRequest);

impl StateFileSuccessorRequest {
    pub fn new(owner_schema: u16, owner_id: impl Into<Vec<u8>>) -> io::Result<Self> {
        let owner_id = owner_id.into();
        if owner_schema == 0 || owner_id.is_empty() || owner_id.len() > 255 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "State successor owner identity is invalid",
            ));
        }
        Ok(Self {
            owner_schema,
            owner_id,
        })
    }

    pub fn owner_schema(&self) -> u16 {
        self.owner_schema
    }

    pub fn owner_id(&self) -> &[u8] {
        &self.owner_id
    }
}

enum StateBatchStage {
    Creating(FileCreateObligation),
    Writing(StagedFile),
    Sealed(SealedStagedFile),
    Discarding(StageDiscardObligation),
}

struct StateBatchMember {
    destination: Option<ReplaceDestination>,
    contents: Vec<u8>,
    name: LeafName,
    old: Option<recovery::RecoveryFileProof>,
    stage: Option<StateBatchStage>,
}

struct StateBatchPreparation {
    id: u64,
    parent: Directory,
    request: StateFileSuccessorRequest,
    members: Vec<StateBatchMember>,
    armed: bool,
}

impl Drop for StateBatchPreparation {
    fn drop(&mut self) {
        if self.armed {
            std::process::abort();
        }
    }
}

struct StateBatchReplay {
    replay: recovery_runtime::RecoveryReplay,
    id: u64,
    parent: Directory,
    targets: Vec<(LeafName, recovery::RecoveryFileProof)>,
}

struct StateBatchFinalization {
    journal: Option<Box<RecoveryJournal>>,
    orphans: Option<Vec<RecoveryOrphan>>,
    id: u64,
    parent: Directory,
    targets: Vec<(LeafName, recovery::RecoveryFileProof)>,
}

impl Drop for StateBatchFinalization {
    fn drop(&mut self) {
        if self.journal.is_some() {
            std::process::abort();
        }
    }
}

enum StateFileBatchState {
    Preparing(StateBatchPreparation),
    Rollback {
        cause: io::Error,
        preparation: StateBatchPreparation,
    },
    Forward {
        preparation: StateBatchPreparation,
        successor: SuccessorOwner,
    },
    Replaying(StateBatchReplay),
    Finalizing(StateBatchFinalization),
}

#[must_use = "State file batch outcomes must be settled"]
pub enum StateFileBatchOutcome {
    Replaced(Vec<FileCapability>),
    NoEffect {
        error: io::Error,
        replacements: Vec<(ReplaceDestination, Vec<u8>)>,
    },
    AppliedUnverified(StateFileBatchObligation),
}

impl_redacted_debug!(StateFileBatchOutcome);

#[must_use = "State file batch obligations must be reconciled"]
pub struct StateFileBatchObligation {
    error: io::Error,
    state: Option<Box<StateFileBatchState>>,
}

impl_redacted_debug!(StateFileBatchObligation);

impl StateFileBatchObligation {
    pub fn error(&self) -> &io::Error {
        &self.error
    }

    pub fn reconcile(mut self) -> StateFileBatchOutcome {
        settle_state_file_batch(
            *self
                .state
                .take()
                .expect("State batch obligation retains its owner"),
        )
    }
}

impl RootStateSuccessor {
    pub fn owner_schema(&self) -> u16 {
        self.descriptor.owner_schema
    }

    pub fn owner_id(&self) -> &[u8] {
        &self.descriptor.owner_id
    }

    pub fn recovery_count(&self) -> usize {
        self.descriptor.recoveries.len()
    }

    pub fn recovery_destination(&self, index: usize) -> Option<(Vec<&str>, &str)> {
        let (_, record) = self.descriptor.recoveries.get(index)?;
        Some((
            record
                .destination_parent
                .iter()
                .map(RecoveryName::as_str)
                .collect(),
            record.destination_leaf.as_str(),
        ))
    }
}

#[must_use = "root acquisition effects must be explicitly acquired, reconciled, cleaned up, or preserved"]
#[derive(Debug)]
#[expect(
    clippy::large_enum_variant,
    reason = "the linear platform lease/startup obligation is a cold once-per-session carrier consumed immediately; it is not stored in a resident collection"
)]
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
#[expect(
    clippy::large_enum_variant,
    reason = "the linear platform lease/startup obligation is a cold once-per-session carrier consumed immediately; it is not stored in a resident collection"
)]
pub enum AdmittedRootSessionAcquireOutcome {
    Acquired(AdmittedRootSession),
    NoEffect(RootSessionError),
    AppliedUnverified(AdmittedRootSessionAcquireObligation),
}

#[must_use = "admitted root acquisition obligation must be reconciled, cleaned up, or preserved"]
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

    #[expect(
        clippy::result_large_err,
        reason = "the 192-byte obligation remains in a once-per-session cleanup loop and is never stored in a resident collection"
    )]
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

    #[expect(
        clippy::result_large_err,
        reason = "the 192-byte obligation remains in a once-per-session acknowledgement path and is never stored in a resident collection"
    )]
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let obligation = self
            .obligation
            .take()
            .expect("admitted root acquisition retains its obligation");
        match obligation.acknowledge_preserved() {
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

    pub fn admit_absolute_directory(&self, path: &Path) -> io::Result<Directory> {
        self.validate_retained_authority()?;
        let directory = self.session.admit_absolute_directory(path)?;
        self.validate_retained_authority()?;
        Ok(directory)
    }

    pub fn validate_retained_authority(&self) -> io::Result<()> {
        self.session.validate_retained_authority()?;
        let admitted_identity =
            platform::directory_identity(&self.admission.directory.inner.handle)?;
        if admitted_identity != self.admission.directory.inner.identity.physical
            || self
                .admission
                .directory
                .inner
                .identity
                .filesystem_identity()
                != self.session.identity().filesystem_identity()
        {
            return Err(identity_changed(
                "admitted root session changed physical identity",
            ));
        }
        Ok(())
    }
}

struct AcquiredRoot {
    lease: platform::LeaseHandle,
    replay: Option<recovery_runtime::RecoveryReplay>,
}

#[must_use = "partial root construction must be reconciled, cleaned up, or preserved"]
pub struct RootSessionAcquireObligation {
    error: RootSessionError,
    construction: Option<platform::RootConstruction>,
    lease: Option<platform::LeaseAcquisitionObligation>,
    acquired: Option<Box<AcquiredRoot>>,
    process_image: Option<platform::ProcessImageAncestry>,
}

impl_redacted_debug!(RootSessionAcquireObligation);

impl RootSessionAcquireObligation {
    pub fn error(&self) -> &RootSessionError {
        &self.error
    }

    pub fn state_successor(&self) -> io::Result<Option<RootStateSuccessor>> {
        let Some(replay) = self
            .acquired
            .as_ref()
            .and_then(|acquired| acquired.replay.as_ref())
        else {
            return Ok(None);
        };
        replay
            .state_successor()
            .map(|descriptor| descriptor.map(|descriptor| RootStateSuccessor { descriptor }))
    }

    pub fn reconcile_state_successor(
        mut self,
        successor: RootStateSuccessor,
    ) -> RootSessionAcquireOutcome {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        let Some(mut acquired) = self.acquired.take() else {
            self.error = RootSessionError::Recovery(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root acquisition has no State successor",
            ));
            self.construction = Some(construction);
            return RootSessionAcquireOutcome::AppliedUnverified(self);
        };
        let Some(replay) = acquired.replay.take() else {
            self.error = RootSessionError::Recovery(io::Error::new(
                io::ErrorKind::InvalidInput,
                "root acquisition has no State successor replay owner",
            ));
            self.construction = Some(construction);
            self.acquired = Some(acquired);
            return RootSessionAcquireOutcome::AppliedUnverified(self);
        };
        let current = replay.state_successor();
        if current.as_ref().ok().and_then(Option::as_ref) != Some(&successor.descriptor) {
            self.error = RootSessionError::Recovery(io::Error::new(
                io::ErrorKind::InvalidInput,
                "State successor admission changed before replay",
            ));
            acquired.replay = Some(replay);
            self.construction = Some(construction);
            self.acquired = Some(acquired);
            return RootSessionAcquireOutcome::AppliedUnverified(self);
        }
        let identity = match platform::root_construction_identity(&construction) {
            Ok(identity) => identity,
            Err(error) => {
                self.error = RootSessionError::Create(error);
                acquired.replay = Some(replay);
                self.construction = Some(construction);
                self.acquired = Some(acquired);
                return RootSessionAcquireOutcome::AppliedUnverified(self);
            }
        };
        let root = match platform::root_construction_guard(&construction) {
            Ok(root) => root,
            Err(error) => {
                self.error = RootSessionError::Create(error);
                acquired.replay = Some(replay);
                self.construction = Some(construction);
                self.acquired = Some(acquired);
                return RootSessionAcquireOutcome::AppliedUnverified(self);
            }
        };
        let process_image = self
            .process_image
            .take()
            .expect("root acquisition obligation retains process image ancestry");
        match replay.resume_state_successor(root, &acquired.lease) {
            Ok(recovery) => finish_root_session_with_recovery(
                construction,
                identity,
                acquired.lease,
                process_image,
                recovery,
            ),
            Err((error, replay)) => {
                self.error = RootSessionError::Recovery(error);
                acquired.replay = Some(replay);
                self.construction = Some(construction);
                self.acquired = Some(acquired);
                self.process_image = Some(process_image);
                RootSessionAcquireOutcome::AppliedUnverified(self)
            }
        }
    }

    pub fn reconcile(mut self) -> RootSessionAcquireOutcome {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(mut acquired) = self.acquired.take() {
            let identity = match platform::root_construction_identity(&construction) {
                Ok(identity) => identity,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.acquired = Some(acquired);
                    return RootSessionAcquireOutcome::AppliedUnverified(self);
                }
            };
            let process_image = self
                .process_image
                .take()
                .expect("root acquisition obligation retains process image ancestry");
            if let Some(replay) = acquired.replay.take() {
                let root = match platform::root_construction_guard(&construction) {
                    Ok(root) => root,
                    Err(error) => {
                        self.error = RootSessionError::Create(error);
                        acquired.replay = Some(replay);
                        self.construction = Some(construction);
                        self.acquired = Some(acquired);
                        self.process_image = Some(process_image);
                        return RootSessionAcquireOutcome::AppliedUnverified(self);
                    }
                };
                return match replay.resume(root, &acquired.lease) {
                    Ok(recovery) => finish_root_session_with_recovery(
                        construction,
                        identity,
                        acquired.lease,
                        process_image,
                        recovery,
                    ),
                    Err((error, replay)) => {
                        self.error = RootSessionError::Recovery(error);
                        acquired.replay = Some(replay);
                        self.construction = Some(construction);
                        self.acquired = Some(acquired);
                        self.process_image = Some(process_image);
                        RootSessionAcquireOutcome::AppliedUnverified(self)
                    }
                };
            }
            return finish_root_session(construction, identity, acquired.lease, process_image);
        }
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
                    return finish_root_session(construction, identity, lease, process_image);
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

    #[expect(
        clippy::result_large_err,
        reason = "the 184-byte obligation remains in a once-per-session cleanup loop and is never stored in a resident collection"
    )]
    pub fn cleanup(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if self.acquired.is_some() {
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

    #[expect(
        clippy::result_large_err,
        reason = "the 184-byte obligation remains in a once-per-session acknowledgement path and is never stored in a resident collection"
    )]
    pub fn acknowledge_preserved(mut self) -> Result<(), Self> {
        let construction = self
            .construction
            .take()
            .expect("root acquisition obligation retains construction");
        if let Some(mut acquired) = self.acquired.take() {
            let root = match platform::root_construction_guard(&construction) {
                Ok(root) => root,
                Err(error) => {
                    self.error = RootSessionError::Create(error);
                    self.construction = Some(construction);
                    self.acquired = Some(acquired);
                    return Err(self);
                }
            };
            if let Err(error) = platform::validate_lease(&acquired.lease)
                .and_then(|()| platform::validate_root(root))
            {
                self.error = RootSessionError::Recovery(error);
                self.construction = Some(construction);
                self.acquired = Some(acquired);
                return Err(self);
            }
            let root = platform::finish_root_construction(construction);
            if let Some(replay) = acquired.replay.take() {
                replay.acknowledge();
            }
            drop(self.process_image.take());
            drop(acquired.lease);
            drop(root);
            return Ok(());
        }
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
        if self.construction.is_some() || self.lease.is_some() || self.acquired.is_some() {
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

fn terminal_effect_settlement_admits_owner(authority: &CapabilityAuthority, owner_id: u64) -> bool {
    TERMINAL_EFFECT_SETTLEMENT_AUTHORITY.with(|slot| {
        slot.get() == Some((authority as *const CapabilityAuthority as usize, owner_id))
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
        .and_then(|count| count.checked_add(state.directory_parks.len()))
        .and_then(|count| count.checked_add(state.moves.len()))
        .and_then(|count| count.checked_add(state.transients.len()))
        .ok_or_else(|| io::Error::other("filesystem effect registry count overflowed"))?;
    if state.outstanding_effects != registered_effects {
        return Err(io::Error::other(
            "filesystem effect registry accounting is inconsistent",
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
    carrier: Option<Box<T>>,
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
            carrier: Some(Box::new(carrier)),
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
                .map(|carrier| *carrier)
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
    FileMoveAfterPark(OwnedFileMoveAfterPark),
    DirectoryMove(OwnedDirectoryMove),
}

enum OwnedFilePromotion {
    Pending {
        obligation: Box<FilePromotionObligation>,
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
        obligation: Box<FileReplaceObligation>,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: Box<FileReplaceTerminal>,
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

enum OwnedFileMoveAfterPark {
    Pending {
        obligation: FileMoveAfterParkObligation,
        receipt_live: ReceiptLiveness,
    },
    Ready {
        terminal: FileMoveAfterParkTerminal,
        receipt_live: ReceiptLiveness,
    },
}

enum FileMoveAfterParkTerminal {
    Applied {
        current: FileCapability,
        displaced: ParkedFile,
    },
    NoEffect {
        source: FileCapability,
        displaced: ParkedFile,
    },
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

#[must_use = "file-move park handoff receipts must be claimed after explicit owner settlement"]
pub struct FileMoveAfterParkReceipt {
    owner: Arc<EffectOwnerState>,
    id: u64,
    live: ReceiptLiveness,
}

impl_redacted_debug!(FileMoveAfterParkReceipt);

#[must_use = "file-move park handoff receipt outcomes must be handled"]
pub enum FileMoveAfterParkReceiptOutcome {
    Pending(FileMoveAfterParkReceipt),
    Applied {
        current: FileCapability,
        displaced: ParkedFile,
    },
    NoEffect {
        source: FileCapability,
        displaced: ParkedFile,
    },
}

impl_redacted_debug!(FileMoveAfterParkReceiptOutcome);

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
        obligation: Box<FilePromotionObligation>,
    ) -> Result<FilePromotionReceipt, EffectOwnerRetentionError<Box<FilePromotionObligation>>> {
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
                    obligation: Box::new(obligation),
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

    pub fn retain_file_move_after_park(
        &self,
        obligation: FileMoveAfterParkObligation,
    ) -> Result<FileMoveAfterParkReceipt, EffectOwnerRetentionError<FileMoveAfterParkObligation>>
    {
        let live = Arc::new(AtomicBool::new(true));
        let record_live = live.clone();
        self.retain(
            obligation,
            |_, anchor, obligation| {
                obligation
                    .movement
                    .file
                    .as_ref()
                    .is_some_and(|file| file.parent.is_within(anchor))
                    && obligation.movement.destination.is_within(anchor)
                    && obligation.displaced.parent.is_within(anchor)
            },
            move |obligation| {
                OwnedEffect::FileMoveAfterPark(OwnedFileMoveAfterPark::Pending {
                    obligation,
                    receipt_live: record_live,
                })
            },
        )
        .map(|id| FileMoveAfterParkReceipt {
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

fn file_park_obligation_is_within(obligation: &FileParkObligation, anchor: &Directory) -> bool {
    obligation
        .request
        .as_ref()
        .is_some_and(|request| request.file.parent.is_within(anchor))
}

fn replace_destination_is_within(destination: &ReplaceDestination, anchor: &Directory) -> bool {
    match destination {
        ReplaceDestination::Vacant { parent, .. } => parent.is_within(anchor),
        ReplaceDestination::Existing(request) => request.file.parent.is_within(anchor),
        ReplaceDestination::Preserved(file) => file.parent.is_within(anchor),
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
    obligation
        .state
        .as_ref()
        .is_some_and(|state| match state.as_ref() {
            FileReplaceObligationState::Parking { park, staged, .. } => {
                file_park_obligation_is_within(park, anchor) && staged.file.parent.is_within(anchor)
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
            Some(TerminalEffectSettlementScope::begin(&authority, self.id)?)
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
            | Self::FileMoveAfterPark(OwnedFileMoveAfterPark::Pending { .. })
            | Self::DirectoryMove(OwnedDirectoryMove::Pending { .. }) => true,
            Self::FilePromotion(OwnedFilePromotion::Ready { receipt_live, .. })
            | Self::FileReplace(OwnedFileReplace::Ready { receipt_live, .. })
            | Self::FileMove(OwnedFileMove::Ready { receipt_live, .. })
            | Self::FileMoveAfterPark(OwnedFileMoveAfterPark::Ready { receipt_live, .. })
            | Self::DirectoryMove(OwnedDirectoryMove::Ready { receipt_live, .. }) => {
                receipt_is_live(receipt_live)
            }
            _ => false,
        }
    }

    fn is_unresolved(&self) -> bool {
        !matches!(
            self,
            Self::FilePromotion(OwnedFilePromotion::Ready { .. })
                | Self::FileReplace(OwnedFileReplace::Ready { .. })
                | Self::FileMove(OwnedFileMove::Ready { .. })
                | Self::FileMoveAfterPark(OwnedFileMoveAfterPark::Ready { .. })
                | Self::DirectoryMove(OwnedDirectoryMove::Ready { .. })
        )
    }

    fn settle(self) -> Option<Self> {
        match self {
            Self::StageCreateCleanup(obligation) => match obligation.reconcile() {
                FileCreateResolution::Created(staged) => owned_stage_discard(staged.discard()),
                FileCreateResolution::NoEffect(_) => None,
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
                FileParkResolution::Preserved { file, .. } => {
                    drop(file);
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
                FileParkResolution::Preserved { file, .. } => {
                    drop(file);
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
                DirectoryParkResolution::Parked(parked) => owned_parked_directory_removal(parked),
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
            Self::ParkedDirectoryTreeRemoval(parked) => owned_parked_directory_tree_removal(parked),
            Self::DirectoryTreeRemoval(obligation) => match obligation.reconcile() {
                DirectoryTreeRemovalResolution::Removed => None,
                DirectoryTreeRemovalResolution::Indeterminate(obligation) => {
                    Some(Self::DirectoryTreeRemoval(obligation))
                }
            },
            Self::DirectoryParkRestore(obligation) => match obligation.restore() {
                DirectoryParkResolution::Parked(parked) => owned_parked_directory_restore(parked),
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
            Self::FileMoveAfterPark(owned) => settle_owned_file_move_after_park(owned),
            Self::DirectoryMove(owned) => settle_owned_directory_move(owned),
        }
    }
}

fn settle_owned_file_promotion(owned: OwnedFilePromotion) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedFilePromotion::Pending {
            obligation,
            receipt_live,
        } => match (*obligation).reconcile() {
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
                terminal: Box::new(FileReplaceTerminal::Replaced { current, displaced }),
                receipt_live,
            },
            FileReplaceResolution::NoEffect {
                staged,
                destination,
            } => OwnedFileReplace::Ready {
                terminal: Box::new(FileReplaceTerminal::NoEffect {
                    staged,
                    destination,
                }),
                receipt_live,
            },
            FileReplaceResolution::Indeterminate(obligation) => OwnedFileReplace::Pending {
                obligation: Box::new(obligation),
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

fn settle_owned_file_move_after_park(owned: OwnedFileMoveAfterPark) -> Option<OwnedEffect> {
    let owned = match owned {
        OwnedFileMoveAfterPark::Pending {
            obligation,
            receipt_live,
        } => match obligation.reconcile() {
            FileMoveAfterParkResolution::Applied { current, displaced } => {
                OwnedFileMoveAfterPark::Ready {
                    terminal: FileMoveAfterParkTerminal::Applied { current, displaced },
                    receipt_live,
                }
            }
            FileMoveAfterParkResolution::NoEffect { source, displaced } => {
                OwnedFileMoveAfterPark::Ready {
                    terminal: FileMoveAfterParkTerminal::NoEffect { source, displaced },
                    receipt_live,
                }
            }
            FileMoveAfterParkResolution::Indeterminate(obligation) => {
                OwnedFileMoveAfterPark::Pending {
                    obligation,
                    receipt_live,
                }
            }
        },
        ready => ready,
    };
    match owned {
        OwnedFileMoveAfterPark::Ready {
            terminal,
            receipt_live,
        } if !receipt_is_live(&receipt_live) => {
            drop(terminal);
            None
        }
        owned => Some(OwnedEffect::FileMoveAfterPark(owned)),
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
        FileRemovalOutcome::NoEffect { parked, .. } => Some(OwnedEffect::ParkedFileRemoval(parked)),
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
        FileRestoreOutcome::NoEffect { parked, .. } => Some(OwnedEffect::ParkedFileRestore(parked)),
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
                match *terminal {
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

impl FileMoveAfterParkReceipt {
    pub fn claim(self) -> FileMoveAfterParkReceiptOutcome {
        let mut records = self
            .owner
            .effects
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let effect = records.effects.remove(&self.id);
        match effect {
            Some(OwnedEffect::FileMoveAfterPark(OwnedFileMoveAfterPark::Ready {
                terminal,
                receipt_live,
            })) if Arc::ptr_eq(&receipt_live, &self.live) && receipt_is_live(&receipt_live) => {
                drop(records);
                deactivate_claimed_owner(&self.owner);
                match terminal {
                    FileMoveAfterParkTerminal::Applied { current, displaced } => {
                        FileMoveAfterParkReceiptOutcome::Applied { current, displaced }
                    }
                    FileMoveAfterParkTerminal::NoEffect { source, displaced } => {
                        FileMoveAfterParkReceiptOutcome::NoEffect { source, displaced }
                    }
                }
            }
            Some(effect) => {
                records.effects.insert(self.id, effect);
                drop(records);
                FileMoveAfterParkReceiptOutcome::Pending(self)
            }
            None => {
                drop(records);
                FileMoveAfterParkReceiptOutcome::Pending(self)
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

impl Drop for FileMoveAfterParkReceipt {
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
    #[cfg(test)]
    directory_open_pause: Mutex<Option<DirectoryOpenReservationPause>>,
    session_nonce: [u8; 16],
    root: platform::RootGuard,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
}

#[cfg(test)]
struct DirectoryOpenReservationPause {
    parent: DirectoryIdentity,
    name: LeafName,
    prechecked: Arc<std::sync::Barrier>,
    resume: Arc<std::sync::Barrier>,
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
    cleanup: Option<platform::FileCleanupHandle>,
    phase: StageRegistryPhase,
    carrier: StageCarrierState,
    promotion: Option<StagePromotionRecord>,
    recovery: Option<recovery::RecoveryRegistration>,
}

struct StagePromotionRecord {
    destination: NamespaceLeaf,
    attempt_id: u64,
    receipt: platform::PublicationReceipt,
    displaced_park: Option<u64>,
}

struct NamespaceLeaf {
    parent: Directory,
    name: LeafName,
}

struct RecoveryOrphan {
    registration: recovery::RecoveryRegistration,
    ancestors: Vec<platform::Identity>,
    files: Vec<platform::Identity>,
}

struct MoveEffectRecord {
    source: NamespaceLeaf,
    destination: NamespaceLeaf,
    moved_directory: Option<platform::Identity>,
    moved_file: Option<platform::Identity>,
    displaced_park: Option<u64>,
}

fn directory_has_physical_ancestor(directory: &Directory, ancestor: platform::Identity) -> bool {
    let mut current = directory;
    loop {
        if current.inner.identity.physical == ancestor {
            return true;
        }
        if current
            .inner
            .absolute_ancestry
            .as_ref()
            .is_some_and(|guard| platform::absolute_directory_has_ancestor(guard, ancestor))
        {
            return true;
        }
        let Some(parent) = current.inner.parent.as_ref() else {
            return false;
        };
        current = &parent.directory;
    }
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
    checked_out: bool,
    recovery: Option<recovery::RecoveryRegistration>,
    recovery_intent: Option<recovery::RecoveryRecord>,
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
        self.record
            .as_ref()
            .expect("stage create guard retains record")
    }

    fn record_mut(&mut self) -> &mut StageCreateRecord {
        self.record
            .as_mut()
            .expect("stage create guard retains record")
    }

    fn disarm(
        mut self,
        token: &mut StageCreateToken,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        let recovery = self.record().recovery;
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        if let Some(registration) = recovery {
            settle_recovery_no_effect(&self.authority.lease, &mut state, registration)?;
        }
        self.record
            .take()
            .expect("stage create guard retains record");
        let removed = state
            .stage_creations
            .remove(&self.id)
            .expect("checked-out stage create header remains registered");
        assert!(removed.checked_out);
        state.release_effect(operation);
        token.armed = false;
        Ok(())
    }

    fn finish_transfer(mut self, token: &StageCreateToken) {
        self.record
            .take()
            .expect("stage create guard retains record");
        assert!(!token.armed);
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
        let header = state
            .stage_creations
            .get_mut(&self.id)
            .expect("checked-out stage create header remains registered");
        assert!(header.checked_out);
        header.created = record.created;
        header.cleanup = record.cleanup;
        header.identity = record.identity;
        header.phase = record.phase;
        header.checked_out = false;
        header.recovery = record.recovery;
        header.recovery_intent = record.recovery_intent;
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
    checked_out: bool,
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
        moved_file: Option<platform::Identity>,
        displaced_park: Option<&FileParkRegistryToken>,
    ) -> io::Result<Self> {
        if !Arc::ptr_eq(authority, &operation.authority) {
            return Err(stale_capability());
        }
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE || state.active == 0 {
            return Err(stale_capability());
        }
        let record = MoveEffectRecord {
            source,
            destination,
            moved_directory,
            moved_file,
            displaced_park: None,
        };
        let displaced_park = match (moved_file, displaced_park) {
            (Some(identity), displaced) => state.file_park_handoff_id(
                displaced,
                Arc::as_ptr(authority),
                &record.destination,
                Some((identity, &record.source)),
            )?,
            (None, None) => None,
            (None, Some(_)) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "directory moves cannot consume a file-park handoff",
                ));
            }
        };
        let record = MoveEffectRecord {
            displaced_park,
            ..record
        };
        if state.namespace_footprint_is_reserved(
            &[
                (&record.source.parent, &record.source.name),
                (&record.destination.parent, &record.destination.name),
            ],
            record
                .moved_directory
                .map(|identity| (identity, &record.source.parent)),
            moved_file,
            displaced_park,
            None,
            None,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "filesystem move conflicts with an unsettled namespace effect",
            ));
        }
        let id = state.reserve_move_effect(record)?;
        if let Some(park_id) = displaced_park {
            let park = state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated move handoff park remains registered");
            assert!(park.linked_effect.is_none());
            park.linked_effect = Some(FileParkLink::Move(id));
        }
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

    fn disarm(mut self, token: &mut DirectoryCreateEffectToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record
            .take()
            .expect("directory create guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .directory_creations
            .remove(&self.id)
            .expect("checked-out directory create header remains registered");
        assert!(removed.checked_out);
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
        let header = state
            .directory_creations
            .get_mut(&self.id)
            .expect("checked-out directory create header remains registered");
        assert!(header.checked_out);
        header.created = record.created;
        header.cleanup = record.cleanup;
        header.identity = record.identity;
        header.phase = record.phase;
        header.checked_out = false;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileParkRegistryPhase {
    Reserved,
    Live,
    Abandoned,
}

struct FileParkRegistryRecord {
    parent: Directory,
    original_name: LeafName,
    name: LeafName,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
    expected_digest: Option<[u8; 32]>,
    cleanup: Option<platform::FileCleanupHandle>,
    phase: FileParkRegistryPhase,
    linked_effect: Option<FileParkLink>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FileParkLink {
    Stage(u64),
    Move(u64),
}

struct FileParkSettlementRecord {
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
    cleanup: Option<platform::DirectoryCleanupHandle>,
    phase: DirectoryParkRegistryPhase,
}

struct DirectoryParkSettlementRecord {
    parent: Directory,
    original_name: LeafName,
    name: LeafName,
    identity: platform::Identity,
    cleanup: platform::DirectoryCleanupHandle,
    phase: DirectoryParkRegistryPhase,
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
    record: Option<FileParkSettlementRecord>,
}

impl FileParkRecordGuard {
    fn record(&self) -> &FileParkSettlementRecord {
        self.record
            .as_ref()
            .expect("file park guard retains record")
    }

    fn record_mut(&mut self) -> &mut FileParkSettlementRecord {
        self.record
            .as_mut()
            .expect("file park guard retains record")
    }

    fn disarm(mut self, token: &mut FileParkRegistryToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record.take().expect("file park guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .file_parks
            .remove(&self.id)
            .expect("checked-out file park header remains registered");
        assert!(removed.cleanup.is_none());
        assert!(removed.linked_effect.is_none());
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for FileParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .file_parks
            .get_mut(&self.id)
            .expect("checked-out file park header remains registered");
        assert!(header.cleanup.is_none());
        header.size = record.size;
        header.stamp = record.stamp;
        header.expected_digest = record.expected_digest;
        header.cleanup = Some(record.cleanup);
        header.phase = record.phase;
    }
}

struct DirectoryParkRecordGuard {
    authority: Arc<CapabilityAuthority>,
    id: u64,
    record: Option<DirectoryParkSettlementRecord>,
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
    fn record(&self) -> &DirectoryParkSettlementRecord {
        self.record
            .as_ref()
            .expect("directory park guard retains record")
    }

    fn record_mut(&mut self) -> &mut DirectoryParkSettlementRecord {
        self.record
            .as_mut()
            .expect("directory park guard retains record")
    }

    fn disarm(mut self, token: &mut DirectoryParkRegistryToken, operation: &CapabilityOperation) {
        assert!(Arc::ptr_eq(&self.authority, &operation.authority));
        self.record
            .take()
            .expect("directory park guard retains record");
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let removed = state
            .directory_parks
            .remove(&self.id)
            .expect("checked-out directory park header remains registered");
        assert!(removed.cleanup.is_none());
        state.release_effect(operation);
        token.armed = false;
    }
}

impl Drop for DirectoryParkRecordGuard {
    fn drop(&mut self) {
        let Some(record) = self.record.take() else {
            return;
        };
        let mut state = self
            .authority
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let header = state
            .directory_parks
            .get_mut(&self.id)
            .expect("checked-out directory park header remains registered");
        assert!(header.cleanup.is_none());
        header.cleanup = Some(record.cleanup);
        header.phase = record.phase;
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
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if (state.phase != AUTHORITY_LIVE
            && !(state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self)))
            || state.active == 0
        {
            return Err(stale_capability());
        }
        let displaced_park = state
            .moves
            .get(&id)
            .ok_or_else(stale_capability)?
            .displaced_park;
        if let Some(park_id) = displaced_park {
            let park = state.file_parks.get(&park_id).ok_or_else(|| {
                io::Error::other("move handoff lost its file-park registry header")
            })?;
            if park.linked_effect != Some(FileParkLink::Move(id)) {
                return Err(io::Error::other("move handoff link is inconsistent"));
            }
        }
        state
            .moves
            .remove(&id)
            .expect("prevalidated move effect remains registered");
        if let Some(park_id) = displaced_park {
            state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated move handoff park remains registered")
                .linked_effect = None;
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
            && !(state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        if state.recovery.is_uncertain() {
            state.recovery.reconcile_uncertain(&self.lease)?;
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
            || state
                .effect_owner_handles
                .get(&owner_id)
                .is_none_or(|owner| owner.strong_count() == 0)
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

    fn enter_effect_retention(self: &Arc<Self>, owner_id: u64) -> io::Result<CapabilityOperation> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE || !state.effect_owner_handles.contains_key(&owner_id) {
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
        state
            .effect_owner_handles
            .insert(id, Arc::downgrade(&owner));
        Ok(EffectOwner { state: owner })
    }

    fn retain_effect_owner_record<T>(
        self: &Arc<Self>,
        owner: &Arc<EffectOwnerState>,
        operation: &CapabilityOperation,
        carrier: T,
        wrap: impl FnOnce(T) -> OwnedEffect,
    ) -> Result<u64, EffectOwnerRetentionError<T>> {
        if !Arc::ptr_eq(self, &operation.authority) || owner.authority.as_ptr() != Arc::as_ptr(self)
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
        let retained = records.effects.len().saturating_add(records.in_flight);
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
            || (state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self));
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
            || (state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self));
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
            && state
                .directory_creations
                .get(&token.id)
                .is_some_and(|record| {
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
            .filter(|record| record.phase == DirectoryCreateEffectPhase::UnclassifiedRecovery)
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
                (record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending)
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

    fn ensure_leaf_not_directory_create_reserved(
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
                && leaf_names_equivalent(record.name.as_os_str(), name.as_os_str())
        }) {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory name is reserved by an unsettled creation",
            ));
        }
        Ok(())
    }

    #[cfg(test)]
    fn pause_directory_open_after_precheck(&self, parent: &Directory, name: &LeafName) {
        let pause = self
            .directory_open_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .as_ref()
            .filter(|pause| {
                pause.parent == parent.inner.identity
                    && leaf_names_equivalent(pause.name.as_os_str(), name.as_os_str())
            })
            .map(|pause| (Arc::clone(&pause.prechecked), Arc::clone(&pause.resume)));
        if let Some((prechecked, resume)) = pause {
            prechecked.wait();
            resume.wait();
        }
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

    fn ensure_leaf_not_root_control(
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
        if parent.inner.identity.physical == state.root_identity
            && leaf_names_equivalent(name.as_os_str(), OsStr::new(ROOT_LEASE_NAME))
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "root recovery control is reserved by the filesystem authority",
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
        creation: &mut StageCreateToken,
        operation: &CapabilityOperation,
    ) -> io::Result<StageToken> {
        if !creation.armed
            || !Arc::ptr_eq(self, &operation.authority)
            || creation.authority.as_ptr() != Arc::as_ptr(self)
        {
            return Err(stale_capability());
        }
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING) {
            return Err(stale_capability());
        }
        let creation_header = state.stage_creations.get(&creation.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
        })?;
        if !creation_header.checked_out
            || creation_header.parent.inner.identity != parent.inner.identity
            || !leaf_names_equivalent(creation_header.name.as_os_str(), name.as_os_str())
        {
            return Err(stale_capability());
        }
        let recovery = creation_header.recovery;
        if state.namespace_footprint_is_reserved(
            &[(&parent, &name)],
            None,
            Some(identity),
            None,
            Some(creation.id),
            None,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by an unsettled filesystem effect",
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
                cleanup: Some(cleanup),
                phase: StageRegistryPhase::Writing,
                carrier: StageCarrierState::Live,
                promotion: None,
                recovery,
            },
        );
        let removed = state
            .stage_creations
            .remove(&creation.id)
            .expect("prevalidated stage create header remains registered");
        assert!(removed.checked_out);
        creation.armed = false;
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
        recovery: Option<recovery::RecoveryRegistration>,
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
        if state.namespace_leaf_is_reserved(parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "stage name is reserved by an unsettled filesystem effect",
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
                checked_out: false,
                recovery,
                recovery_intent: None,
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
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let header = state.stage_creations.get_mut(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
        })?;
        if header.checked_out {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage create settlement is already in progress",
            ));
        }
        header.checked_out = true;
        let record = StageCreateRecord {
            parent: header.parent.clone(),
            name: header.name.clone(),
            created: header.created.take(),
            cleanup: header.cleanup.take(),
            identity: header.identity,
            phase: header.phase,
            checked_out: true,
            recovery: header.recovery,
            recovery_intent: header.recovery_intent.clone(),
        };
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
        if state.namespace_leaf_is_reserved(parent, name) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory name is reserved by an unsettled filesystem effect",
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
                checked_out: false,
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
        let identity = platform::directory_identity(&created).ok();
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let record = state
            .directory_creations
            .get_mut(&token.id)
            .expect("admitted directory create reservation remains registered");
        record.created = Some(created);
        record.identity = identity;
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
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let header = state
            .directory_creations
            .get_mut(&token.id)
            .ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
        if header.checked_out {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory create settlement is already in progress",
            ));
        }
        header.checked_out = true;
        let record = DirectoryCreateEffectRecord {
            parent: header.parent.clone(),
            name: header.name.clone(),
            created: header.created.take(),
            cleanup: header.cleanup.take(),
            identity: header.identity,
            phase: header.phase,
            checked_out: true,
        };
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
        parent: &Directory,
        original_name: &LeafName,
        park_name: &LeafName,
        directory_identity: Option<platform::Identity>,
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
        if state.namespace_footprint_is_reserved(
            &[(parent, original_name), (parent, park_name)],
            directory_identity.map(|identity| (identity, parent)),
            None,
            None,
            None,
            None,
        ) {
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
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if state.namespace_footprint_is_reserved(
            &[(parent, &original_name), (parent, &park_name)],
            None,
            Some(identity),
            None,
            None,
            None,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "file park name is reserved by an unsettled filesystem effect",
            ));
        }
        let id = state.next_file_park_id;
        state.next_file_park_id = state
            .next_file_park_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("file park registry identity overflowed"))?;
        state.reserve_effect()?;
        assert!(
            state
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
                        cleanup: Some(cleanup),
                        phase,
                        linked_effect: None,
                    },
                )
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
        let header = state
            .file_parks
            .get_mut(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        if header.linked_effect.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park is linked to an unsettled replacement",
            ));
        }
        let cleanup = header.cleanup.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park is already checked out",
            )
        })?;
        let record = FileParkSettlementRecord {
            parent: header.parent.clone(),
            original_name: header.original_name.clone(),
            name: header.name.clone(),
            identity: header.identity,
            size: header.size,
            stamp: header.stamp,
            expected_digest: header.expected_digest,
            cleanup,
            phase: header.phase,
        };
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
        let record = state
            .file_parks
            .remove(&token.id)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "file park record is absent"))?;
        if record.linked_effect.is_some() || record.cleanup.is_none() {
            state.file_parks.insert(token.id, record);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "file park authority is not available for rollback",
            ));
        }
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
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE {
            return Err(stale_capability());
        }
        if state.namespace_footprint_is_reserved(
            &[(parent, &original_name), (parent, &park_name)],
            Some((identity, parent)),
            None,
            None,
            None,
            None,
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "directory park name is reserved by an unsettled filesystem effect",
            ));
        }
        let id = state.next_directory_park_id;
        state.next_directory_park_id = state
            .next_directory_park_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("directory park registry identity overflowed"))?;
        state.reserve_effect()?;
        assert!(
            state
                .directory_parks
                .insert(
                    id,
                    DirectoryParkRegistryRecord {
                        parent: parent.clone(),
                        original_name,
                        name: park_name,
                        identity,
                        cleanup: Some(cleanup),
                        phase,
                    },
                )
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
        let header = state.directory_parks.get_mut(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
        })?;
        let cleanup = header.cleanup.take().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory park is already checked out",
            )
        })?;
        let record = DirectoryParkSettlementRecord {
            parent: header.parent.clone(),
            original_name: header.original_name.clone(),
            name: header.name.clone(),
            identity: header.identity,
            cleanup,
            phase: header.phase,
        };
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
        let record = state.directory_parks.remove(&token.id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "directory park record is absent")
        })?;
        if record.cleanup.is_none() {
            state.directory_parks.insert(token.id, record);
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "directory park authority is not available for rollback",
            ));
        }
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
        let displaced_park = state
            .stages
            .get(&id)
            .and_then(|record| record.promotion.as_ref())
            .and_then(|promotion| promotion.displaced_park);
        if matches!(
            phase,
            StageRegistryPhase::Writing | StageRegistryPhase::Sealed
        ) && let Some(park_id) = displaced_park
        {
            let park = state.file_parks.get_mut(&park_id).ok_or_else(|| {
                io::Error::other("replacement park link lost its registry header")
            })?;
            if park.linked_effect != Some(FileParkLink::Stage(id)) {
                return Err(io::Error::other("replacement park link is inconsistent"));
            }
            park.linked_effect = None;
        }
        let record = state.stages.get_mut(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        record.phase = phase;
        if matches!(
            phase,
            StageRegistryPhase::Writing | StageRegistryPhase::Sealed
        ) {
            record.promotion = None;
        }
        Ok(())
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the registry transition validates every publication proof coordinate together"
    )]
    fn prepare_stage_promotion(
        &self,
        id: u64,
        destination: Directory,
        name: LeafName,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
        displaced_park: Option<&FileParkRegistryToken>,
        expected_recovery: Option<&RecoveryRecord>,
    ) -> io::Result<()> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if !(matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
            || state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self))
        {
            return Err(stale_capability());
        }
        if !receipt.matches_attempt(attempt_id) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "publication receipt does not match the allocated stage attempt",
            ));
        }
        let displaced_park_id = state.file_park_handoff_id(
            displaced_park,
            self,
            &NamespaceLeaf {
                parent: destination.clone(),
                name: name.clone(),
            },
            None,
        )?;
        if state.namespace_footprint_is_reserved(
            &[(&destination, &name)],
            None,
            None,
            displaced_park_id,
            None,
            Some(id),
        ) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "promotion destination is reserved by an unsettled filesystem effect",
            ));
        }
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.phase != StageRegistryPhase::Sealed || record.promotion.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage is not ready for a new publication attempt",
            ));
        }
        prepare_recovery_publication(&self.lease, &mut state, id, expected_recovery)?;
        let record = state
            .stages
            .get_mut(&id)
            .expect("prevalidated stage remains registered");
        record.promotion = Some(StagePromotionRecord {
            destination: NamespaceLeaf {
                parent: destination,
                name,
            },
            attempt_id,
            receipt,
            displaced_park: displaced_park_id,
        });
        record.phase = StageRegistryPhase::PromotionAttempted;
        if let Some(park_id) = displaced_park_id {
            let park = state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated replacement park remains registered");
            assert!(park.linked_effect.is_none());
            park.linked_effect = Some(FileParkLink::Stage(id));
        }
        Ok(())
    }

    fn record_stage_publication(
        &self,
        id: u64,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
    ) {
        let mut state = self
            .operations
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let Some(record) = state.stages.get_mut(&id) else {
            std::process::abort();
        };
        if record.phase != StageRegistryPhase::PromotionAttempted {
            std::process::abort();
        }
        let Some(promotion) = record.promotion.as_mut() else {
            std::process::abort();
        };
        if promotion.attempt_id != attempt_id
            || !receipt.matches_attempt(attempt_id)
            || !promotion.receipt.accepts_successor(&receipt)
        {
            std::process::abort();
        }
        promotion.receipt = receipt;
    }

    fn validate_stage_publication(
        &self,
        id: u64,
        attempt_id: u64,
        receipt: &platform::PublicationReceipt,
    ) -> io::Result<()> {
        let state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let record = state.stages.get(&id).ok_or_else(stale_capability)?;
        let promotion = record.promotion.as_ref().ok_or_else(stale_capability)?;
        if record.phase != StageRegistryPhase::PromotionAttempted
            || promotion.attempt_id != attempt_id
            || !receipt.matches_attempt(attempt_id)
            || !promotion.receipt.accepts_successor(receipt)
        {
            return Err(stale_capability());
        }
        Ok(())
    }

    fn allocate_stage_publication_attempt(&self, id: u64) -> io::Result<u64> {
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.phase != StageRegistryPhase::Sealed || record.promotion.is_some() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage is not ready for a new publication attempt",
            ));
        }
        let attempt_id = state.next_publication_attempt_id;
        state.next_publication_attempt_id = attempt_id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("publication attempt identity overflowed"))?;
        Ok(attempt_id)
    }

    fn disarm_stage(self: &Arc<Self>, id: u64, operation: &CapabilityOperation) -> io::Result<()> {
        assert!(Arc::ptr_eq(&operation.authority, self));
        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        let record = state.stages.get(&id).ok_or_else(|| {
            io::Error::new(io::ErrorKind::NotFound, "stage registry entry is absent")
        })?;
        if record.cleanup.is_none() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "stage settlement is already in progress",
            ));
        }
        let displaced_park = record
            .promotion
            .as_ref()
            .and_then(|promotion| promotion.displaced_park);
        if let Some(park_id) = displaced_park {
            let park = state.file_parks.get(&park_id).ok_or_else(|| {
                io::Error::other("replacement park link lost its registry header")
            })?;
            if park.linked_effect != Some(FileParkLink::Stage(id)) {
                return Err(io::Error::other("replacement park link is inconsistent"));
            }
        }
        state
            .stages
            .remove(&id)
            .expect("prevalidated stage remains registered");
        if let Some(park_id) = displaced_park {
            let park = state
                .file_parks
                .get_mut(&park_id)
                .expect("prevalidated replacement park remains registered");
            park.linked_effect = None;
        }
        state.release_effect(operation);
        Ok(())
    }

    fn cleanup_stage(self: &Arc<Self>, id: u64) -> io::Result<()> {
        let (mut record, operation) = {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if !(matches!(state.phase, AUTHORITY_LIVE | AUTHORITY_DRAINING)
                || state.phase == AUTHORITY_QUIESCING && terminal_effect_settlement_admits(self))
            {
                return Err(stale_capability());
            }
            if state.recovery.is_uncertain() {
                state.recovery.reconcile_uncertain(&self.lease)?;
            }
            if !state.stages.contains_key(&id) {
                return Ok(());
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .stages
                .get_mut(&id)
                .expect("prevalidated stage header remains registered");
            let cleanup = header.cleanup.take().ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "stage cleanup is already in progress",
                )
            })?;
            let record = StageRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                identity: header.identity,
                cleanup: Some(cleanup),
                phase: header.phase,
                carrier: header.carrier,
                promotion: header
                    .promotion
                    .as_ref()
                    .map(|promotion| StagePromotionRecord {
                        destination: NamespaceLeaf {
                            parent: promotion.destination.parent.clone(),
                            name: promotion.destination.name.clone(),
                        },
                        attempt_id: promotion.attempt_id,
                        receipt: promotion.receipt.clone(),
                        displaced_park: promotion.displaced_park,
                    }),
                recovery: header.recovery,
            };
            state.active = active;
            (
                record,
                CapabilityOperation {
                    authority: self.clone(),
                },
            )
        };
        let mut expected_recovery = None;
        let result = platform::validate_lease(&self.lease)
            .and_then(|()| platform::validate_root(&self.root))
            .and_then(|()| record.parent.validate(&operation))
            .and_then(|()| match record.phase {
                StageRegistryPhase::PromotionAttempted | StageRegistryPhase::Unresolved => {
                    let promotion = record.promotion.as_mut().ok_or_else(|| {
                        io::Error::other("promotion stage lost its destination authority")
                    })?;
                    let destination = &promotion.destination;
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
                        (
                            platform::BindingState::Exact,
                            platform::BindingState::Absent | platform::BindingState::Occupied,
                        ) if promotion.receipt.is_attempted() => {
                            expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                            platform::remove_parked_file(
                                &record.parent.inner.handle,
                                record.name.as_os_str(),
                                record
                                    .cleanup
                                    .as_mut()
                                    .expect("checked-out stage retains cleanup authority"),
                                record.identity,
                            )
                        }
                        (platform::BindingState::Absent, platform::BindingState::Exact) => {
                            expected_recovery =
                                selected_recovery_stage_record(self, record.recovery)?;
                            platform::settle_parked_publication(
                                &mut promotion.receipt,
                                promotion.attempt_id,
                                record
                                    .cleanup
                                    .as_ref()
                                    .expect("checked-out stage retains cleanup authority"),
                                &record.parent.inner.handle,
                                record.name.as_os_str(),
                                &destination.parent.inner.handle,
                                destination.name.as_os_str(),
                            )?;
                            destination.parent.validate(&operation)
                        }
                        _ => Err(identity_changed(
                            "promotion-attempted stage topology is indeterminate",
                        )),
                    }
                }
                StageRegistryPhase::CleanupAttempted => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    if platform::settle_removed_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_ref()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                    .is_ok()
                    {
                        Ok(())
                    } else {
                        platform::remove_parked_file(
                            &record.parent.inner.handle,
                            record.name.as_os_str(),
                            record
                                .cleanup
                                .as_mut()
                                .expect("checked-out stage retains cleanup authority"),
                            record.identity,
                        )
                    }
                }
                StageRegistryPhase::Writing => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    platform::remove_parked_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_mut()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                }
                StageRegistryPhase::Sealed => {
                    expected_recovery = prepare_recovery_stage_removal(self, &record)?;
                    platform::remove_parked_file(
                        &record.parent.inner.handle,
                        record.name.as_os_str(),
                        record
                            .cleanup
                            .as_mut()
                            .expect("checked-out stage retains cleanup authority"),
                        record.identity,
                    )
                }
            })
            .and_then(|()| {
                settle_removed_recovery_stage(self, &operation, &record, expected_recovery.as_ref())
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
            let header = state
                .stages
                .get_mut(&id)
                .expect("checked-out stage header remains registered");
            assert!(header.cleanup.is_none());
            header.cleanup = record.cleanup;
            header.phase = record.phase;
            header.carrier = record.carrier;
            header.promotion = record.promotion;
            header.recovery = record.recovery;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .stages
                .remove(&id)
                .expect("settled stage header remains registered");
            assert!(removed.cleanup.is_none());
            if let Some(park_id) = removed
                .promotion
                .as_ref()
                .and_then(|promotion| promotion.displaced_park)
            {
                let park = state
                    .file_parks
                    .get_mut(&park_id)
                    .expect("linked replacement park remains registered");
                assert_eq!(park.linked_effect, Some(FileParkLink::Stage(id)));
                park.linked_effect = None;
            }
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
            let header = state.stage_creations.get(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "stage create record is absent")
            })?;
            if !matches!(
                header.phase,
                StageCreatePhase::Abandoned | StageCreatePhase::CleanupAttempted
            ) || header.checked_out
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "stage create authority is still live",
                ));
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .stage_creations
                .get_mut(&id)
                .expect("prevalidated stage create header remains registered");
            header.checked_out = true;
            let record = StageCreateRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                created: header.created.take(),
                cleanup: header.cleanup.take(),
                identity: header.identity,
                phase: header.phase,
                checked_out: true,
                recovery: header.recovery,
                recovery_intent: header.recovery_intent.clone(),
            };
            state.active = active;
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
            })
            .and_then(|()| settle_removed_recovery_stage_create(self, &operation, &record));
        if let Err(error) = result {
            record.phase = StageCreatePhase::CleanupAttempted;
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let header = state
                .stage_creations
                .get_mut(&id)
                .expect("checked-out stage create header remains registered");
            assert!(header.checked_out);
            header.created = record.created;
            header.cleanup = record.cleanup;
            header.identity = record.identity;
            header.phase = record.phase;
            header.checked_out = false;
            header.recovery = record.recovery;
            header.recovery_intent = record.recovery_intent;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .stage_creations
                .remove(&id)
                .expect("settled stage create header remains registered");
            assert!(removed.checked_out);
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
            let header = state.directory_creations.get(&id).ok_or_else(|| {
                io::Error::new(io::ErrorKind::NotFound, "directory create record is absent")
            })?;
            if !matches!(
                header.phase,
                DirectoryCreateEffectPhase::Abandoned
                    | DirectoryCreateEffectPhase::CleanupAttempted
            ) || header.checked_out
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "directory create authority is still live",
                ));
            }
            let active = state.active.checked_add(1).ok_or_else(|| {
                io::Error::other("filesystem capability operation count overflowed")
            })?;
            let header = state
                .directory_creations
                .get_mut(&id)
                .expect("prevalidated directory create header remains registered");
            header.checked_out = true;
            let record = DirectoryCreateEffectRecord {
                parent: header.parent.clone(),
                name: header.name.clone(),
                created: header.created.take(),
                cleanup: header.cleanup.take(),
                identity: header.identity,
                phase: header.phase,
                checked_out: true,
            };
            state.active = active;
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
            let header = state
                .directory_creations
                .get_mut(&id)
                .expect("checked-out directory create header remains registered");
            assert!(header.checked_out);
            header.created = record.created;
            header.cleanup = record.cleanup;
            header.identity = record.identity;
            header.phase = record.phase;
            header.checked_out = false;
            drop(state);
            return Err(error);
        }
        {
            assert!(Arc::ptr_eq(&operation.authority, self));
            let mut state = self
                .operations
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            let removed = state
                .directory_creations
                .remove(&id)
                .expect("settled directory create header remains registered");
            assert!(removed.checked_out);
            state.release_effect(&operation);
        }
        Ok(())
    }

    fn begin_terminal_drain(&self, require_empty_recovery: bool) -> io::Result<()> {
        {
            let mut state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_LIVE {
                return Err(stale_capability());
            }
            if state.recovery.is_uncertain()
                || require_empty_recovery && state.recovery.has_live_or_uncertain()
                || state.state_batch.is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "root recovery control retains unsettled ownership",
                ));
            }
            if state.active != 0 {
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

        let mut state = self
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_QUIESCING {
            return Err(stale_capability());
        }
        if state.active != 0 || !state.active_effect_owners.is_empty() {
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
        let (cleanup_ids, create_cleanup_ids, directory_create_cleanup_ids, transient_cleanup_ids) = {
            let state = self.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_DRAINING {
                return Err(stale_capability());
            }
            if state.active != 0 || !state.moves.is_empty() {
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
                        (record.phase == transient::TransientEffectPhase::Abandoned).then_some(*id)
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
            || state.state_batch.is_some()
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
                            record.phase == DirectoryCreateEffectPhase::UnclassifiedAbandoned
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
            || !state.transients.is_empty()
            || state.state_batch.is_some()
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
        if terminal_phase == AUTHORITY_RESETTING && state.recovery.has_live_or_uncertain() {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "root reset is blocked by durable recovery ownership",
            ));
        }
        let directory_creations_settled = state.directory_creations.is_empty()
            || (terminal_phase == AUTHORITY_RESETTING
                && state.directory_creations.len() == reset_pending_count
                && state.directory_creations.values().all(|record| {
                    record.phase == DirectoryCreateEffectPhase::CreatedUnclassifiedResetPending
                }));
        if state.active != 0
            || state.outstanding_effects != expected_outstanding
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !directory_creations_settled
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.transients.is_empty()
            || state.state_batch.is_some()
        {
            return Ok(SessionDrainSettlement::Pending);
        }
        state.phase = terminal_phase;
        Ok(SessionDrainSettlement::Ready)
    }
}

struct OperationState {
    phase: u8,
    root_identity: platform::Identity,
    active: usize,
    outstanding_effects: usize,
    next_move_id: u64,
    moves: HashMap<u64, MoveEffectRecord>,
    next_effect_owner_id: u64,
    effect_owner_handles: HashMap<u64, Weak<EffectOwnerState>>,
    active_effect_owners: HashMap<u64, Arc<EffectOwnerState>>,
    next_stage_id: u64,
    next_publication_attempt_id: u64,
    stages: HashMap<u64, StageRecord>,
    next_stage_create_id: u64,
    stage_creations: HashMap<u64, StageCreateRecord>,
    next_directory_create_id: u64,
    directory_creations: HashMap<u64, DirectoryCreateEffectRecord>,
    next_file_park_id: u64,
    file_parks: HashMap<u64, FileParkRegistryRecord>,
    next_directory_park_id: u64,
    directory_parks: HashMap<u64, DirectoryParkRegistryRecord>,
    next_transient_id: u64,
    transients: HashMap<u64, transient::TransientEffectRecord>,
    state_batch: Option<u64>,
    recovery: recovery::RecoveryJournal,
    recovery_orphans: Vec<RecoveryOrphan>,
}

impl OperationState {
    fn file_park_handoff_id(
        &self,
        token: Option<&FileParkRegistryToken>,
        authority: *const CapabilityAuthority,
        destination: &NamespaceLeaf,
        moved_file: Option<(platform::Identity, &NamespaceLeaf)>,
    ) -> io::Result<Option<u64>> {
        let Some(token) = token else {
            return Ok(None);
        };
        if !token.armed || token.authority.as_ptr() != authority {
            return Err(stale_capability());
        }
        let park = self
            .file_parks
            .get(&token.id)
            .ok_or_else(stale_capability)?;
        let source_conflicts = moved_file.is_some_and(|(identity, source)| {
            identity == park.identity
                || source.parent.inner.identity == park.parent.inner.identity
                    && (leaf_names_equivalent(
                        source.name.as_os_str(),
                        park.original_name.as_os_str(),
                    ) || leaf_names_equivalent(source.name.as_os_str(), park.name.as_os_str()))
        });
        if park.phase != FileParkRegistryPhase::Live
            || park.linked_effect.is_some()
            || park.cleanup.is_none()
            || park.parent.inner.identity != destination.parent.inner.identity
            || !leaf_names_equivalent(park.original_name.as_os_str(), destination.name.as_os_str())
            || source_conflicts
        {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "replacement park cannot hand off its original leaf",
            ));
        }
        Ok(Some(token.id))
    }

    fn namespace_footprint_is_reserved(
        &self,
        candidate_leaves: &[(&Directory, &LeafName)],
        candidate_subtree: Option<(platform::Identity, &Directory)>,
        candidate_file: Option<platform::Identity>,
        excluded_file_park_original: Option<u64>,
        excluded_stage_create: Option<u64>,
        excluded_recovery_target_stage: Option<u64>,
    ) -> bool {
        if candidate_leaves.iter().any(|(directory, name)| {
            directory.inner.identity.physical == self.root_identity
                && leaf_names_equivalent(name.as_os_str(), OsStr::new(ROOT_LEASE_NAME))
        }) {
            return true;
        }
        let candidate_conflicts_with_name = |directory: &Directory, name: &OsStr| {
            candidate_leaves
                .iter()
                .any(|(candidate_directory, candidate_name)| {
                    candidate_directory.inner.identity == directory.inner.identity
                        && leaf_names_equivalent(name, candidate_name.as_os_str())
                })
                || candidate_subtree.is_some_and(|(identity, _)| {
                    directory_has_physical_ancestor(directory, identity)
                })
        };
        let candidate_conflicts_with_subtree =
            |identity: platform::Identity, anchor: &Directory| {
                candidate_leaves
                    .iter()
                    .any(|(directory, _)| directory_has_physical_ancestor(directory, identity))
                    || candidate_subtree.is_some_and(|(candidate_identity, candidate_anchor)| {
                        candidate_identity == identity
                            || directory_has_physical_ancestor(candidate_anchor, identity)
                            || directory_has_physical_ancestor(anchor, candidate_identity)
                    })
            };

        let excluded_recovery = excluded_stage_create
            .and_then(|id| self.stage_creations.get(&id))
            .and_then(|record| record.recovery);
        if self.recovery_orphans.iter().any(|orphan| {
            if Some(orphan.registration) == excluded_recovery {
                return false;
            }
            let Some(record) = self.recovery.record(orphan.registration) else {
                return true;
            };
            let Some(parent) = orphan.ancestors.last().copied() else {
                return true;
            };
            let owns_target = record.phase.owns_target();
            let stage = recovery::recovery_stage_leaf(record.operation_id);
            let park = recovery::recovery_park_leaf(record.operation_id);
            let owns_leaf = candidate_leaves.iter().any(|(directory, name)| {
                directory.inner.identity.physical == parent
                    && (leaf_names_equivalent(name.as_os_str(), OsStr::new(stage.as_str()))
                        || recovery_owns_park(record)
                            && leaf_names_equivalent(name.as_os_str(), OsStr::new(park.as_str()))
                        || owns_target
                            && leaf_names_equivalent(
                                name.as_os_str(),
                                OsStr::new(record.destination_leaf.as_str()),
                            ))
            });
            let owns_subtree =
                candidate_subtree.is_some_and(|(identity, _)| orphan.ancestors.contains(&identity));
            let owns_file = candidate_file.is_some_and(|identity| orphan.files.contains(&identity));
            owns_leaf || owns_subtree || owns_file
        }) {
            return true;
        }

        self.moves.values().any(|movement| {
            candidate_file.is_some() && candidate_file == movement.moved_file
                || candidate_conflicts_with_name(
                    &movement.source.parent,
                    movement.source.name.as_os_str(),
                )
                || candidate_conflicts_with_name(
                    &movement.destination.parent,
                    movement.destination.name.as_os_str(),
                )
                || movement.moved_directory.is_some_and(|identity| {
                    candidate_conflicts_with_subtree(identity, &movement.source.parent)
                })
        }) || self.transients.values().any(|record| {
            candidate_file.is_some() && candidate_file == record.identity
                || candidate_conflicts_with_name(&record.directory, record.destination.as_os_str())
        }) || self.directory_creations.values().any(|record| {
            candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || record.identity.is_some_and(|identity| {
                    candidate_conflicts_with_subtree(identity, &record.parent)
                })
                || record.identity.is_none()
        }) || self.stage_creations.iter().any(|(id, record)| {
            Some(*id) != excluded_stage_create
                && (candidate_file.is_some() && candidate_file == record.identity
                    || candidate_conflicts_with_name(&record.parent, record.name.as_os_str()))
        }) || self.file_parks.iter().any(|(id, record)| {
            candidate_file == Some(record.identity)
                || (*id != excluded_file_park_original.unwrap_or(u64::MAX)
                    && candidate_conflicts_with_name(
                        &record.parent,
                        record.original_name.as_os_str(),
                    ))
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
        }) || self.directory_parks.values().any(|record| {
            candidate_conflicts_with_name(&record.parent, record.original_name.as_os_str())
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || candidate_conflicts_with_subtree(record.identity, &record.parent)
        }) || self.stages.iter().any(|(id, record)| {
            candidate_file == Some(record.identity)
                || candidate_conflicts_with_name(&record.parent, record.name.as_os_str())
                || record.promotion.as_ref().is_some_and(|promotion| {
                    candidate_conflicts_with_name(
                        &promotion.destination.parent,
                        promotion.destination.name.as_os_str(),
                    )
                })
                || (Some(*id) != excluded_recovery_target_stage
                    && record.recovery.is_some_and(|registration| {
                        self.recovery.record(registration).is_none_or(|recovery| {
                            recovery.phase.owns_target()
                                && candidate_conflicts_with_name(
                                    &record.parent,
                                    OsStr::new(recovery.destination_leaf.as_str()),
                                )
                        })
                    }))
        })
    }

    fn namespace_leaf_is_reserved(
        &self,
        candidate_directory: &Directory,
        candidate_name: &LeafName,
    ) -> bool {
        self.namespace_footprint_is_reserved(
            &[(candidate_directory, candidate_name)],
            None,
            None,
            None,
            None,
            None,
        )
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
        assert!(
            state.active > 0,
            "filesystem capability operation count underflowed"
        );
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

    fn prepare_promotion(
        &self,
        destination: &Directory,
        name: &LeafName,
        attempt_id: u64,
        receipt: platform::PublicationReceipt,
        displaced_park: Option<&FileParkRegistryToken>,
        expected_recovery: Option<&RecoveryRecord>,
    ) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .prepare_stage_promotion(
                self.id,
                destination.clone(),
                name.clone(),
                attempt_id,
                receipt,
                displaced_park,
                expected_recovery,
            )
    }

    fn allocate_publication_attempt(&self) -> io::Result<u64> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .allocate_stage_publication_attempt(self.id)
    }

    fn record_publication(&self, attempt_id: u64, receipt: platform::PublicationReceipt) {
        let Some(authority) = self.authority.upgrade() else {
            std::process::abort();
        };
        authority.record_stage_publication(self.id, attempt_id, receipt);
    }

    fn validate_publication_attempt(
        &self,
        attempt_id: u64,
        receipt: &platform::PublicationReceipt,
    ) -> io::Result<()> {
        self.authority
            .upgrade()
            .ok_or_else(stale_capability)?
            .validate_stage_publication(self.id, attempt_id, receipt)
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
        let authority = self.authority.upgrade().ok_or_else(stale_capability)?;
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
        Ok(
            match RootSession::acquire_absolute_directory_guard(ancestry) {
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
            },
        )
    }
}

impl fmt::Debug for Directory {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("Directory").finish_non_exhaustive()
    }
}

fn live_recovery_record(
    state: &OperationState,
    registration: RecoveryRegistration,
) -> io::Result<RecoveryRecord> {
    state
        .recovery
        .record(registration)
        .cloned()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid recovery record"))
}

fn selected_recovery_stage_record(
    authority: &CapabilityAuthority,
    registration: Option<RecoveryRegistration>,
) -> io::Result<Option<RecoveryRecord>> {
    let Some(registration) = registration else {
        return Ok(None);
    };
    let state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
    Ok(Some(live_recovery_record(&state, registration)?))
}

fn prepare_recovery_stage_removal(
    authority: &CapabilityAuthority,
    stage: &StageRecord,
) -> io::Result<Option<RecoveryRecord>> {
    let Some(registration) = stage.recovery else {
        return Ok(None);
    };
    let mut state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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

fn clear_recovery(
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

fn settle_recovery_no_effect(
    lease: &platform::LeaseHandle,
    state: &mut OperationState,
    registration: RecoveryRegistration,
) -> io::Result<()> {
    clear_recovery(lease, state, registration)
}

#[cfg(windows)]
fn retain_recovery_orphan(
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

fn settle_removed_recovery(
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
        let state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        match state.recovery.record(registration) {
            None => return Ok(()),
            Some(current) if current == expected => {}
            Some(_) => return Err(stale_capability()),
        }
        drop(state);
        platform::sync_publication_directory(&parent.inner.handle)?;
        parent.validate(operation)?;
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        parent.validate(operation)?;
        if state.recovery.record(registration) != Some(expected) {
            return Err(stale_capability());
        }
        clear_recovery(&authority.lease, &mut state, registration)
    }
    #[cfg(windows)]
    {
        parent.validate(operation)?;
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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
        retain_recovery_orphan(
            &mut state,
            registration,
            parent,
            target.into_iter().collect(),
        );
        Ok(())
    }
}

fn settle_removed_recovery_stage(
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
                &promotion.destination.parent.inner.handle,
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
                &parent.inner.handle,
                OsStr::new(expected.destination_leaf.as_str()),
            )?;
            let proof = recovery_runtime::prove_file(
                &parent.inner.handle,
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

fn settle_removed_recovery_stage_create(
    authority: &CapabilityAuthority,
    operation: &CapabilityOperation,
    record: &StageCreateRecord,
) -> io::Result<()> {
    match record.recovery {
        Some(registration) => {
            let expected = authority
                .operations
                .lock()
                .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?
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

fn validate_recovery_stage_writable(
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
    let state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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

fn seal_recovery_stage(
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
        let state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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
    let proof = recovery_runtime::prove_file(
        &file.parent.inner.handle,
        file.name.as_os_str(),
        &file.handle,
        file.identity,
    )?;
    file.validate_revision_in(operation, revision)?;
    if proof.size != revision.size {
        return Err(identity_changed("recoverable stage revision changed"));
    }
    let mut state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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
    if state.state_batch.as_ref().is_some() {
        if current.phase != RecoveryPhase::StagePrepared || current.new.is_some() {
            return Err(stale_capability());
        }
        let _ = proof;
        return Ok(());
    }
    match current.phase {
        RecoveryPhase::StagePrepared if current.new.is_none() => {
            let destination = recovery_leaf(&current.destination_leaf)?;
            if state.namespace_footprint_is_reserved(
                &[(&parent, &destination)],
                None,
                None,
                None,
                None,
                None,
            ) {
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

#[expect(
    clippy::too_many_arguments,
    reason = "recovery admission binds the complete live publication carrier in one check"
)]
fn validate_recovery_publication(
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
    let state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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
    let proof = recovery_runtime::prove_file(
        &source.inner.handle,
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

fn prepare_recovery_publication(
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

fn complete_recovery_publication(
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
        let state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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
    let proof = recovery_runtime::prove_file(
        &destination.inner.handle,
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
        let state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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

fn recovery_parent_components(directory: &Directory) -> io::Result<Vec<RecoveryName>> {
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

fn recovery_leaf(name: &RecoveryName) -> io::Result<LeafName> {
    LeafName::new(name.as_str())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidData, "invalid recovery name"))
}

fn preflight_recovery_create(
    parent: &Directory,
    stage: &LeafName,
    park: Option<&LeafName>,
) -> io::Result<()> {
    let listing = platform::entries(&parent.inner.handle, MAX_DIRECTORY_LIST_ENTRIES)?;
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

fn reconcile_recovery_stage_create(
    authority: &Arc<CapabilityAuthority>,
    token: &mut StageCreateToken,
) -> io::Result<Option<(Directory, LeafName)>> {
    let mut state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
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

impl Directory {
    pub fn replace_state_batch_durable(
        &self,
        request: StateFileSuccessorRequest,
        replacements: Vec<(ReplaceDestination, Vec<u8>)>,
    ) -> StateFileBatchOutcome {
        let admitted = (|| -> io::Result<_> {
            if !(1..=32).contains(&replacements.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "State batch must contain between one and 32 destinations",
                ));
            }
            let authority = self.authority()?;
            let operation = authority.enter()?;
            self.validate(&operation)?;
            let mut declarations = Vec::with_capacity(replacements.len());
            let mut new_bytes = 0_u64;
            for (destination, contents) in &replacements {
                let size = u64::try_from(contents.len())
                    .map_err(|_| io::Error::other("State batch member size does not fit u64"))?;
                new_bytes = new_bytes.checked_add(size).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidInput, "State batch size overflowed")
                })?;
                if size > recovery::MAX_RECOVERABLE_FILE_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "State batch member exceeds the recovery bound",
                    ));
                }
                let (parent, name, old) =
                    validate_state_replace_destination(destination, &operation)?;
                if parent.inner.identity != self.inner.identity
                    || declarations.iter().any(|(prior, _): &(LeafName, _)| {
                        leaf_names_equivalent(prior.as_os_str(), name.as_os_str())
                    })
                {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "State batch destinations must be distinct leaves in one directory",
                    ));
                }
                declarations.push((name, old));
            }
            let old_bytes = declarations.iter().try_fold(0_u64, |total, (_, old)| {
                total.checked_add(old.map_or(0, |proof| proof.size))
            });
            if old_bytes
                .and_then(|total| total.checked_add(new_bytes))
                .is_none_or(|total| total > recovery::MAX_LIVE_PROOF_BYTES)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "State batch prior content exceeds the recovery bound",
                ));
            }
            let mut state = authority.operations.lock().map_err(|_| {
                io::Error::other("filesystem capability operation lock was poisoned")
            })?;
            if state.phase != AUTHORITY_LIVE
                || state.recovery.is_uncertain()
                || state.recovery.records().next().is_some()
                || state.recovery.has_live_successor()
                || state.state_batch.is_some()
            {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another recoverable State publication is active",
                ));
            }
            if !state.recovery.admits_state_batch(declarations.len()) {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "State batch recovery capacity is exhausted",
                ));
            }
            let id = state.next_stage_create_id;
            state.next_stage_create_id = id
                .checked_add(1)
                .ok_or_else(|| io::Error::other("State batch identity overflowed"))?;
            state.state_batch = Some(id);
            Ok((id, declarations))
        })();
        let (id, declarations) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => {
                return StateFileBatchOutcome::NoEffect {
                    error,
                    replacements,
                };
            }
        };
        let members = replacements
            .into_iter()
            .zip(declarations)
            .map(|((destination, contents), (name, old))| StateBatchMember {
                destination: Some(destination),
                contents,
                name,
                old,
                stage: None,
            })
            .collect();
        settle_state_file_batch(StateFileBatchState::Preparing(StateBatchPreparation {
            id,
            parent: self.clone(),
            request,
            members,
            armed: true,
        }))
    }

    pub fn create_recoverable_stage(&self, destination: &LeafName) -> FileCreateOutcome {
        self.create_recoverable_stage_with_old(destination, None, None)
    }

    pub fn create_recoverable_replacement_stage(
        &self,
        destination: &FileParkRequest,
    ) -> FileCreateOutcome {
        match self.recovery_destination_proof(destination) {
            Ok(proof) => {
                self.create_recoverable_stage_with_old(&destination.file.name, Some(proof), None)
            }
            Err(error) => FileCreateOutcome::NoEffect(error),
        }
    }

    fn recovery_destination_proof(
        &self,
        destination: &FileParkRequest,
    ) -> io::Result<recovery::RecoveryFileProof> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)
            .and_then(|()| destination.file.validate_bound_to(self, &operation))
            .and_then(|()| destination.validate_revision(&operation))?;
        let proof = match recovery_runtime::prove_file(
            &self.inner.handle,
            destination.file.name.as_os_str(),
            &destination.file.handle,
            destination.file.identity,
        ) {
            Ok(proof)
                if proof.size == destination.expected.revision.size
                    && proof.sha256 == destination.expected.sha256 =>
            {
                proof
            }
            Ok(_) => {
                return Err(identity_changed(
                    "replacement destination changed during recovery admission",
                ));
            }
            Err(error) => return Err(error),
        };
        destination.validate_revision(&operation)?;
        Ok(proof)
    }

    fn create_recoverable_stage_with_old(
        &self,
        destination: &LeafName,
        old: Option<recovery::RecoveryFileProof>,
        state_batch: Option<u64>,
    ) -> FileCreateOutcome {
        use rand::RngCore as _;

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
        let destination = match destination
            .as_os_str()
            .to_str()
            .and_then(|name| RecoveryName::new_exact(name.to_owned()).ok())
        {
            Some(destination) => destination,
            None => {
                return FileCreateOutcome::NoEffect(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "invalid recovery destination",
                ));
            }
        };
        let parent = match recovery_parent_components(self) {
            Ok(parent) => parent,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        for _ in 0..MAX_STAGE_ATTEMPTS {
            let mut operation_id = [0; 16];
            rand::rngs::OsRng.fill_bytes(&mut operation_id);
            if operation_id == [0; 16] {
                continue;
            }
            let stage = recovery_leaf(&recovery_stage_leaf(operation_id))
                .expect("derived recovery stage is valid");
            let park = recovery_leaf(&recovery_park_leaf(operation_id))
                .expect("derived recovery park is valid");
            let record = RecoveryRecord {
                operation_id,
                phase: RecoveryPhase::StagePrepared,
                destination_parent: parent.clone(),
                destination_leaf: destination.clone(),
                old,
                new: None,
            };
            let mut state = match authority.operations.lock() {
                Ok(state) => state,
                Err(_) => {
                    return FileCreateOutcome::NoEffect(io::Error::other(
                        "filesystem capability operation lock was poisoned",
                    ));
                }
            };
            if state.phase != AUTHORITY_LIVE || state.recovery.is_uncertain() {
                return FileCreateOutcome::NoEffect(stale_capability());
            }
            let batch_admits = match (state_batch, state.state_batch.as_ref()) {
                (None, None) => state.recovery.records().next().is_none(),
                (Some(id), Some(batch)) => {
                    id == *batch && state.recovery.records().take(32).count() < 32
                }
                _ => false,
            };
            if !batch_admits || state.recovery.has_live_successor() {
                return FileCreateOutcome::NoEffect(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "another recoverable State publication is active",
                ));
            }
            let leaves = if old.is_some() {
                vec![(self, &stage), (self, &park)]
            } else {
                vec![(self, &stage)]
            };
            if state.namespace_footprint_is_reserved(&leaves, None, None, None, None, None) {
                continue;
            }
            if let Err(error) =
                preflight_recovery_create(self, &stage, old.is_some().then_some(&park))
            {
                if error.kind() == io::ErrorKind::AlreadyExists {
                    continue;
                }
                return FileCreateOutcome::NoEffect(error);
            }
            let registration = match state.recovery.reserve(&record) {
                Ok(registration) => registration,
                Err(error) => return FileCreateOutcome::NoEffect(error),
            };
            let id = state.next_stage_create_id;
            let Some(next_id) = id.checked_add(1) else {
                return FileCreateOutcome::NoEffect(io::Error::other("stage create id overflowed"));
            };
            if let Err(error) = state.reserve_effect() {
                return FileCreateOutcome::NoEffect(error);
            }
            state.next_stage_create_id = next_id;
            state.stage_creations.insert(
                id,
                StageCreateRecord {
                    parent: self.clone(),
                    name: stage.clone(),
                    created: None,
                    cleanup: None,
                    identity: None,
                    phase: StageCreatePhase::Reserved,
                    checked_out: false,
                    recovery: Some(registration),
                    recovery_intent: Some(record.clone()),
                },
            );
            let mut token = StageCreateToken {
                id,
                authority: Arc::downgrade(&authority),
                armed: true,
            };
            if let Err(error) =
                state
                    .recovery
                    .create_reserved(&authority.lease, registration, record)
            {
                if state.recovery.is_uncertain() {
                    drop(state);
                    return FileCreateOutcome::AppliedUnverified(FileCreateObligation {
                        error,
                        token,
                    });
                }
                state.stage_creations.remove(&id);
                state.release_effect(&operation);
                token.armed = false;
                return FileCreateOutcome::NoEffect(error);
            }
            state
                .stage_creations
                .get_mut(&id)
                .expect("persisted recovery carrier remains registered")
                .recovery_intent = None;
            drop(state);
            return execute_stage_create(self, &stage, &authority, &operation, token);
        }
        FileCreateOutcome::NoEffect(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve a unique recoverable stage",
        ))
    }

    pub fn validate_absolute_projection(&self, path: &Path) -> io::Result<()> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory projection is not absolute",
            ));
        }
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let ancestry = platform::open_absolute_directory_guard(path)?;
        if platform::absolute_directory_identity(&ancestry) != self.inner.identity.physical {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "directory projection does not match its retained capability",
            ));
        }
        self.validate(&operation)
    }

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
            && platform::leaf_names_equal(source_name.as_os_str(), destination_name.as_os_str())
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
            None,
            None,
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
            Err(directory) => DirectoryMoveOutcome::AppliedUnverified(DirectoryMoveObligation {
                error: effect
                    .err()
                    .unwrap_or_else(|| io::Error::other("directory move could not be classified")),
                directory: Some(directory),
                destination: destination.clone(),
                destination_name: destination_name.clone(),
                reported_success,
                token,
            }),
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
        if let Err(error) = authority.ensure_park_available(
            &operation,
            &parent,
            &original_name,
            &park_name,
            Some(self.inner.identity.physical),
        ) {
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
        self.validate_for_authority(&operation.authority)
    }

    fn validate_for_authority(&self, authority: &Arc<CapabilityAuthority>) -> io::Result<()> {
        let mut current = self.inner.as_ref();
        loop {
            if current.authority.as_ptr() != Arc::as_ptr(authority) {
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
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        DirectoryRemovalOutcome::NoEffect {
                            error,
                            parked: self,
                        }
                    }
                    _ => DirectoryRemovalOutcome::AppliedUnverified(DirectoryRemovalObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    fn remove_empty_with_recovery(self, permit: &DrainRecoveryPermit) -> DirectoryRemovalOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        self.remove_empty_admitted(authority, operation)
    }

    fn remove_empty_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRemovalOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRemovalOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRemovalOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        DirectoryRemovalOutcome::NoEffect {
                            error,
                            parked: self,
                        }
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
            return DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                error,
                parked: Some(self),
            });
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
            Ok(()) => DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                error: identity_changed("directory tree removal lost its authority chain"),
                parked: Some(self),
            }),
            Err(error) => {
                DirectoryTreeRemovalOutcome::Indeterminate(DirectoryTreeRemovalObligation {
                    error,
                    parked: Some(self),
                })
            }
        }
    }

    pub fn restore(mut self) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_directory_park(&self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        DirectoryRestoreOutcome::NoEffect {
                            error,
                            parked: self,
                        }
                    }
                    _ => DirectoryRestoreOutcome::AppliedUnverified(DirectoryRestoreObligation {
                        error,
                        parked: Some(self),
                    }),
                }
            }
        }
    }

    fn restore_with_recovery(self, permit: &DrainRecoveryPermit) -> DirectoryRestoreOutcome {
        let authority = match self.authority() {
            Ok(authority) => authority,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        let operation = match authority.enter_directory_park_recovery(permit, &self.token) {
            Ok(operation) => operation,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
        };
        self.restore_admitted(authority, operation)
    }

    fn restore_admitted(
        mut self,
        authority: Arc<CapabilityAuthority>,
        operation: CapabilityOperation,
    ) -> DirectoryRestoreOutcome {
        if let Err(error) = self.validate(&operation) {
            return DirectoryRestoreOutcome::NoEffect {
                error,
                parked: self,
            };
        }
        let mut guard = match authority.take_directory_park(&operation, &self.token) {
            Ok(guard) => guard,
            Err(error) => {
                return DirectoryRestoreOutcome::NoEffect {
                    error,
                    parked: self,
                };
            }
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
                        DirectoryRestoreOutcome::NoEffect {
                            error,
                            parked: self,
                        }
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
        self,
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
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
        #[cfg(test)]
        authority.pause_directory_open_after_precheck(self, name);
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
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
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
            authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)
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
            #[cfg(windows)]
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
            Err(error) => DirectoryCreateOutcome::AppliedUnverified(DirectoryCreateObligation {
                error,
                token: reservation,
            }),
        }
    }

    pub fn open_file(&self, name: &LeafName) -> io::Result<FileCapability> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_root_control(&operation, self, name)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
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
            authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)
        {
            return FileCreateOutcome::NoEffect(error);
        }
        let reservation = match authority.reserve_stage_create(&operation, self, name, None) {
            Ok(reservation) => reservation,
            Err(error) => return FileCreateOutcome::NoEffect(error),
        };
        execute_stage_create(self, name, &authority, &operation, reservation)
    }

    pub fn create_stage(&self) -> FileCreateOutcome {
        use rand::RngCore;

        for _ in 0..MAX_STAGE_ATTEMPTS {
            let mut nonce = [0_u8; 16];
            rand::rngs::OsRng.fill_bytes(&mut nonce);
            let name = format!(".axial-stage-{}", hex::encode(nonce));
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

    /// Creates one link inside an application-owned unpublished tree.
    ///
    /// The caller must retain authority to remove the complete containing tree
    /// until that tree is published. `self` is the containing tree root,
    /// `parent` locates the destination parent beneath it, and `target` is
    /// resolved from that parent. Creation is refused unless the target stays
    /// beneath `self` and resolves to an existing non-link entry.
    #[cfg(unix)]
    pub fn create_owned_symlink_beneath(
        &self,
        parent: &[LeafName],
        name: &LeafName,
        target: &OsStr,
    ) -> io::Result<()> {
        let resolved_target = resolve_owned_symlink_target_beneath(parent, target)?;
        self.validate_owned_symlink_target(&resolved_target)?;
        let destination_parent = self.open_owned_directory_path(parent)?;
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        destination_parent.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(
            &operation,
            &destination_parent,
            name,
        )?;
        authority.ensure_leaf_not_transient_reserved(&operation, &destination_parent, name)?;
        let listing =
            platform::entries(&destination_parent.inner.handle, MAX_DIRECTORY_LIST_ENTRIES)?;
        if !listing.complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owned symlink parent exceeds its bounded namespace",
            ));
        }
        if listing
            .entries
            .iter()
            .any(|(observed, _)| leaf_names_equivalent(observed, name.as_os_str()))
        {
            return Err(io::ErrorKind::AlreadyExists.into());
        }
        platform::create_symlink(&destination_parent.inner.handle, name.as_os_str(), target)?;
        let readback = platform::read_symlink(&destination_parent.inner.handle, name.as_os_str())?;
        if readback != target {
            return Err(identity_changed(
                "owned symlink target changed during creation",
            ));
        }
        platform::sync_directory(&destination_parent.inner.handle)?;
        destination_parent.validate(&operation)?;
        self.validate(&operation)?;
        self.validate_owned_symlink_target(&resolved_target)?;
        if platform::read_symlink(&destination_parent.inner.handle, name.as_os_str())? != target {
            return Err(identity_changed(
                "owned symlink target changed after synchronization",
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn open_owned_directory_path(&self, components: &[LeafName]) -> io::Result<Self> {
        let mut directory = self.clone();
        for component in components {
            directory = directory.open_directory(component)?;
        }
        Ok(directory)
    }

    #[cfg(unix)]
    fn validate_owned_symlink_target(&self, components: &[LeafName]) -> io::Result<()> {
        let (name, parent) = components.split_last().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned symlink target must name an entry beneath its root",
            )
        })?;
        let directory = self.open_owned_directory_path(parent)?;
        let listing = directory.entries(MAX_DIRECTORY_LIST_ENTRIES)?;
        if listing.state() != DirectoryListingState::Complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "owned symlink target parent exceeds its bounded namespace",
            ));
        }
        match listing
            .entries()
            .iter()
            .find(|entry| leaf_names_equivalent(entry.name(), name.as_os_str()))
        {
            Some(entry) if matches!(entry.kind(), EntryKind::File | EntryKind::Directory) => Ok(()),
            Some(_) => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "owned symlink target is not a file or directory",
            )),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                "owned symlink target does not exist",
            )),
        }
    }

    #[cfg(unix)]
    pub fn read_symlink(&self, name: &LeafName) -> io::Result<OsString> {
        let authority = self.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, name)?;
        authority.ensure_leaf_not_transient_reserved(&operation, self, name)?;
        let first = platform::read_symlink(&self.inner.handle, name.as_os_str())?;
        self.validate(&operation)?;
        let second = platform::read_symlink(&self.inner.handle, name.as_os_str())?;
        if first != second {
            return Err(identity_changed(
                "symlink target changed during observation",
            ));
        }
        self.validate(&operation)?;
        Ok(second)
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
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, &name)?;
        #[cfg(test)]
        authority.pause_directory_open_after_precheck(self, &name);
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
        authority.ensure_leaf_not_directory_create_reserved(&operation, self, &name)?;
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
            return Err(identity_changed("existing file park topology is not exact"));
        }
        authority.ensure_park_available(
            &operation,
            self,
            original_name,
            &parked.file.name,
            None,
        )?;
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
            let proof =
                verify_parked_file(self, &park_name, &guard.record().cleanup, &parked.expected);
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
        authority.ensure_park_available(
            &operation,
            self,
            original_name,
            &park_name,
            Some(parked.inner.identity.physical),
        )?;
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

    pub fn park_file_as(&self, request: FileParkRequest, park_name: LeafName) -> FileParkOutcome {
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
        if let Err(error) =
            authority.ensure_park_available(&operation, self, &request.file.name, &park_name, None)
        {
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
        let mut token =
            match authority.reserve_file_park(&operation, &request, park_name.clone(), cleanup) {
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
                    #[cfg(test)]
                    restored_proof_pause: None,
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
                    #[cfg(test)]
                    restored_proof_pause: None,
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

impl PartialEq for FileRevisionObservation {
    fn eq(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.authority, &other.authority)
            && self.identity == other.identity
            && self.size == other.size
            && self.stamp == other.stamp
    }
}

impl Eq for FileRevisionObservation {}

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
    fn new(error: io::Error, file: FileCapability, expected: FileRevision, max_bytes: u64) -> Self {
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

    #[expect(
        clippy::result_large_err,
        reason = "the 144-byte public failure is immediately retried or unpacked; boxing would allocate again on repeated reader admission failure"
    )]
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

    #[expect(
        clippy::result_large_err,
        reason = "the 152-byte public failure is immediately retried or unpacked; boxing would allocate again on repeated reader validation failure"
    )]
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

    pub fn has_same_rename_stable_metadata(&self, other: &Self) -> bool {
        Weak::ptr_eq(&self.authority, &other.authority)
            && self.identity == other.identity
            && self.size == other.size
            && platform::file_content_stamp_matches(self.stamp, other.stamp)
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
    pub fn into_parts(self) -> (FileCapability, FileRevision, [u8; 32]) {
        (self.file, self.expected.revision, self.expected.sha256)
    }

    fn validate_revision(&self, operation: &CapabilityOperation) -> io::Result<()> {
        self.file
            .validate_revision_in(operation, &self.expected.revision)
    }

    fn validate_authority_after_namespace_change(
        &self,
        operation: &CapabilityOperation,
    ) -> io::Result<()> {
        self.file.parent.validate(operation)?;
        if self.file.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || self.expected.revision.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || platform::file_identity(&self.file.handle)? != self.file.identity
            || self.expected.revision.identity != self.file.identity
        {
            return Err(identity_changed(
                "file authority changed across its namespace transition",
            ));
        }
        Ok(())
    }

    #[expect(
        clippy::result_large_err,
        reason = "the 168-byte public failure is immediately unpacked to recover the request and is never stored"
    )]
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
    #[cfg(unix)]
    pub fn make_executable(&self) -> io::Result<()> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        platform::make_file_executable(&self.handle)?;
        self.validate(&operation)
    }

    #[cfg(unix)]
    pub fn is_executable(&self) -> io::Result<bool> {
        let authority = self.parent.authority()?;
        let operation = authority.enter()?;
        self.validate(&operation)?;
        let executable = platform::file_is_executable(&self.handle)?;
        self.validate(&operation)?;
        Ok(executable)
    }

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
        self.move_no_replace_internal(destination, destination_name, None)
    }

    pub fn move_no_replace_after_park(
        self,
        destination: &Directory,
        destination_name: &LeafName,
        displaced: ParkedFile,
    ) -> FileMoveAfterParkOutcome {
        match self.move_no_replace_internal(destination, destination_name, Some(&displaced)) {
            FileMoveOutcome::Applied(current) => {
                FileMoveAfterParkOutcome::Applied { current, displaced }
            }
            FileMoveOutcome::NoEffect { error, file } => FileMoveAfterParkOutcome::NoEffect {
                error,
                source: file,
                displaced,
            },
            FileMoveOutcome::AppliedUnverified(movement) => {
                FileMoveAfterParkOutcome::AppliedUnverified(FileMoveAfterParkObligation {
                    movement,
                    displaced,
                })
            }
        }
    }

    fn move_no_replace_internal(
        self,
        destination: &Directory,
        destination_name: &LeafName,
        displaced: Option<&ParkedFile>,
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
            Some(self.identity),
            displaced.map(|parked| &parked.token),
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

    fn validate_content_revision_in(
        &self,
        operation: &CapabilityOperation,
        expected: &FileRevision,
    ) -> io::Result<()> {
        if self.authority.as_ptr() != Arc::as_ptr(&operation.authority) {
            return Err(stale_capability());
        }
        let (size, stamp) = platform::file_receipt_fields(&self.handle)?;
        if expected.authority.as_ptr() != Arc::as_ptr(&operation.authority)
            || expected.identity != self.identity
            || expected.size != size
            || !platform::file_content_stamp_matches(expected.stamp, stamp)
        {
            return Err(identity_changed("file content revision changed"));
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
                .ok_or_else(|| {
                    io::Error::new(io::ErrorKind::InvalidData, "file cursor overflowed")
                })?;
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

    #[expect(
        clippy::result_large_err,
        reason = "the 144-byte failure preserves a hot-path file capability for immediate retry without allocating on admission failure"
    )]
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
        let allowed = usize::try_from(remaining.min(bytes.len() as u64)).map_err(|_| {
            io::Error::other("file revision read length does not fit this platform")
        })?;
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
    #[expect(
        clippy::result_large_err,
        reason = "the 152-byte failure preserves the hot-path armed reader for immediate retry without allocating on validation failure"
    )]
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
        validate_recovery_stage_writable(&authority, &operation, &self.file, &self.token)?;
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

    /// Makes the staged bytes durable and advances the file to sealed authority.
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
        let (size, stamp) = match platform::file_receipt_fields(&self.file.handle) {
            Ok(receipt) => receipt,
            Err(error) => return Err(StageSealFailure::new(error, self)),
        };
        let revision = FileRevision {
            authority: self.file.authority.clone(),
            identity: self.file.identity,
            size,
            stamp,
        };
        if let Err(error) = platform::sync_publication_file(&self.file.handle) {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.file.validate_revision_in(&operation, &revision) {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) =
            seal_recovery_stage(&authority, &operation, &self.file, &revision, &self.token)
        {
            return Err(StageSealFailure::new(error, self));
        }
        if let Err(error) = self.token.update(StageRegistryPhase::Sealed) {
            return Err(StageSealFailure::new(error, self));
        }
        Ok(SealedStagedFile {
            file: self.file,
            token: self.token,
            revision,
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
    revision: FileRevision,
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
        let Self {
            file,
            token,
            revision: _,
        } = self;
        StagedFile { file, token }.discard()
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
                    FileParkOutcome::NoEffect { error, request } => FileReplaceOutcome::NoEffect {
                        error,
                        staged: self,
                        destination: ReplaceDestination::Existing(request),
                    },
                    FileParkOutcome::Preserved { error, file } => FileReplaceOutcome::NoEffect {
                        error,
                        staged: self,
                        destination: ReplaceDestination::Preserved(file),
                    },
                    FileParkOutcome::AppliedUnverified(park) => {
                        FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                            error: io::Error::other(
                                "replacement destination park is not yet settled",
                            ),
                            state: Some(Box::new(FileReplaceObligationState::Parking {
                                park,
                                staged: self,
                                receipt,
                            })),
                        })
                    }
                }
            }
            ReplaceDestination::Preserved(file) => FileReplaceOutcome::NoEffect {
                error: identity_changed(
                    "preserved replacement destination requires fresh content admission",
                ),
                staged: self,
                destination: ReplaceDestination::Preserved(file),
            },
        }
    }

    pub fn promote_no_replace(
        self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
    ) -> FilePromotionOutcome {
        self.promote_no_replace_internal(source_parent, destination_parent, destination_name, None)
    }

    fn promote_no_replace_internal(
        mut self,
        source_parent: &Directory,
        destination_parent: &Directory,
        destination_name: &LeafName,
        displaced: Option<&ParkedFile>,
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
        if let Err(error) = self.file.validate_revision_in(&operation, &self.revision) {
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
        let expected_recovery = match validate_recovery_publication(
            &authority,
            &operation,
            &self.token,
            &self.file,
            &self.revision,
            source_parent,
            destination_parent,
            destination_name,
            displaced,
        ) {
            Ok(expected) => expected,
            Err(error) => {
                return FilePromotionOutcome::NoEffect {
                    error,
                    staged: self,
                };
            }
        };
        let attempt_id = match self.token.allocate_publication_attempt() {
            Ok(attempt_id) => attempt_id,
            Err(error) => {
                return FilePromotionOutcome::NoEffect {
                    error,
                    staged: self,
                };
            }
        };
        let mut attempt = match platform::prepare_publication(
            attempt_id,
            &self.file.handle,
            self.revision.size,
            self.revision.stamp,
            &source_parent.inner.handle,
            self.file.name.as_os_str(),
            &destination_parent.inner.handle,
            destination_name.as_os_str(),
        ) {
            Ok(attempt) => attempt,
            Err(error) => {
                return FilePromotionOutcome::NoEffect {
                    error,
                    staged: self,
                };
            }
        };
        if let Err(error) = self.token.prepare_promotion(
            destination_parent,
            destination_name,
            attempt_id,
            attempt.clone(),
            displaced.map(|parked| &parked.token),
            expected_recovery.as_ref(),
        ) {
            return FilePromotionOutcome::NoEffect {
                error,
                staged: self,
            };
        }

        let rename = platform::rename_no_replace(
            &mut attempt,
            attempt_id,
            &source_parent.inner.handle,
            self.file.name.as_os_str(),
            &self.file.handle,
            &destination_parent.inner.handle,
            destination_name.as_os_str(),
        );
        let (mut rename_error, mut receipt) = match rename {
            Ok(()) => {
                self.token.record_publication(attempt_id, attempt.clone());
                (None, attempt)
            }
            Err(error) => (Some(error), attempt),
        };
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

        match (source, destination) {
            (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
                if let Err(error) = self
                    .token
                    .validate_publication_attempt(attempt_id, &receipt)
                {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
                }
                let settlement = platform::settle_publication(
                    &mut receipt,
                    attempt_id,
                    &self.file.handle,
                    &source_parent.inner.handle,
                    self.file.name.as_os_str(),
                    &destination_parent.inner.handle,
                    destination_name.as_os_str(),
                );
                self.token.record_publication(attempt_id, receipt.clone());
                if let Err(error) = settlement {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
                }
                if let Err(error) = complete_recovery_publication(
                    &authority,
                    &operation,
                    &self.token,
                    &self.file,
                    &self.revision,
                    destination_parent,
                    destination_name,
                ) {
                    return FilePromotionOutcome::AppliedUnverified(Box::new(
                        FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        },
                    ));
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
                            || applied
                                .validate_content_revision_in(&operation, &self.revision)
                                .is_err()
                            || self.token.disarm().is_err()
                        {
                            FilePromotionOutcome::AppliedUnverified(Box::new(
                                FilePromotionObligation {
                                    error: identity_changed(
                                        "promoted file lost its authority chain",
                                    ),
                                    retained: self,
                                    destination: destination_parent.clone(),
                                    destination_name: destination_name.clone(),
                                    attempt_id,
                                    receipt,
                                },
                            ))
                        } else {
                            FilePromotionOutcome::Applied(applied)
                        }
                    }
                    Ok(_) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error: identity_changed(
                                "promoted file changed before read capability admission",
                            ),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                    Err(error) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error,
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                }
            }
            (
                Ok(platform::BindingState::Exact),
                Ok(platform::BindingState::Absent | platform::BindingState::Occupied),
            ) if receipt.is_attempted() => {
                let error = rename_error.take().unwrap_or_else(|| {
                    identity_changed("promotion reported success without changing topology")
                });
                match self.token.update(StageRegistryPhase::Sealed) {
                    Ok(()) => FilePromotionOutcome::NoEffect {
                        error,
                        staged: self,
                    },
                    Err(update) => {
                        FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                            error: io::Error::other(format!(
                                "promotion failed and stage registry could not settle: {error}; {update}"
                            )),
                            retained: self,
                            destination: destination_parent.clone(),
                            destination_name: destination_name.clone(),
                            attempt_id,
                            receipt,
                        }))
                    }
                }
            }
            _ => FilePromotionOutcome::AppliedUnverified(Box::new(FilePromotionObligation {
                error: rename_error.take().unwrap_or_else(|| {
                    identity_changed("file promotion effect could not be verified")
                }),
                retained: self,
                destination: destination_parent.clone(),
                destination_name: destination_name.clone(),
                attempt_id,
                receipt,
            })),
        }
    }
}

fn validate_state_replace_destination(
    destination: &ReplaceDestination,
    operation: &CapabilityOperation,
) -> io::Result<(Directory, LeafName, Option<recovery::RecoveryFileProof>)> {
    let (parent, name, existing) = match destination {
        ReplaceDestination::Vacant { parent, name } => (parent, name, None),
        ReplaceDestination::Existing(request) => {
            request
                .file
                .validate_bound_to(&request.file.parent, operation)?;
            request.validate_revision(operation)?;
            (&request.file.parent, &request.file.name, Some(request))
        }
        ReplaceDestination::Preserved(_) => {
            return Err(identity_changed(
                "preserved State destination requires fresh content admission",
            ));
        }
    };
    parent.validate(operation)?;
    let before = platform::directory_revision(&parent.inner.handle)?;
    let listing = platform::entries(&parent.inner.handle, MAX_DIRECTORY_LIST_ENTRIES)?;
    if !listing.complete {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "State destination name-class scan exceeded its bound",
        ));
    }
    let mut matches = listing
        .entries
        .iter()
        .filter(|(candidate, _)| leaf_names_equivalent(candidate, name.as_os_str()));
    let matched = matches.next();
    if matches.next().is_some()
        || matched.is_some_and(|(actual, _)| actual.as_os_str() != name.as_os_str())
    {
        return Err(identity_changed(
            "State destination acquired a portable alias",
        ));
    }
    match (existing, matched) {
        (None, None) => {}
        (Some(request), Some((_, EntryKind::File)))
            if platform::file_binding_state(
                &parent.inner.handle,
                name.as_os_str(),
                request.file.identity,
            )? == platform::BindingState::Exact => {}
        (None, Some(_)) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "State destination is occupied",
            ));
        }
        _ => return Err(identity_changed("State destination changed identity")),
    }
    let proof = existing
        .map(|request| {
            let observed = recovery_runtime::prove_file(
                &parent.inner.handle,
                name.as_os_str(),
                &request.file.handle,
                request.file.identity,
            )?;
            if observed.size != request.expected.revision.size
                || observed.sha256 != request.expected.sha256
            {
                return Err(identity_changed("State destination changed content"));
            }
            request.validate_revision(operation)?;
            Ok(observed)
        })
        .transpose()?;
    if platform::directory_revision(&parent.inner.handle)? != before {
        return Err(identity_changed(
            "State destination changed during admission",
        ));
    }
    parent.validate(operation)?;
    Ok((parent.clone(), name.clone(), proof))
}

fn observe_state_batch_stage(
    authority: &Arc<CapabilityAuthority>,
    staged: &SealedStagedFile,
) -> io::Result<(recovery::RecoveryFileProof, (u64, platform::FileStamp))> {
    staged.file.parent.validate_for_authority(authority)?;
    let proof = recovery_runtime::prove_file(
        &staged.file.parent.inner.handle,
        staged.file.name.as_os_str(),
        &staged.file.handle,
        staged.file.identity,
    )?;
    let receipt = platform::file_receipt_fields(&staged.file.handle)?;
    if proof.size != staged.revision.size
        || staged.revision.authority.as_ptr() != Arc::as_ptr(authority)
        || staged.revision.identity != staged.file.identity
        || receipt != (staged.revision.size, staged.revision.stamp)
    {
        return Err(identity_changed("State batch stage changed identity"));
    }
    Ok((proof, receipt))
}

fn validate_state_batch_stage(
    state: &OperationState,
    authority: &Arc<CapabilityAuthority>,
    batch: &StateBatchPreparation,
    member: &StateBatchMember,
    staged: &SealedStagedFile,
) -> io::Result<(RecoveryRegistration, RecoveryRecord)> {
    let stage = state
        .stages
        .get(&staged.token.id)
        .ok_or_else(stale_capability)?;
    let registration = stage.recovery.ok_or_else(stale_capability)?;
    let physical = live_recovery_record(state, registration)?;
    if !staged.token.armed
        || staged.token.authority.as_ptr() != Arc::as_ptr(authority)
        || stage.phase != StageRegistryPhase::Sealed
        || stage.carrier != StageCarrierState::Live
        || stage.identity != staged.file.identity
        || stage.parent.inner.identity != batch.parent.inner.identity
        || stage.name != staged.file.name
        || stage.promotion.is_some()
        || physical.phase != RecoveryPhase::StagePrepared
        || physical.new.is_some()
        || physical.old != member.old
        || physical.destination_parent != recovery_parent_components(&batch.parent)?
        || physical.destination_leaf.as_str()
            != member
                .name
                .as_os_str()
                .to_str()
                .ok_or_else(stale_capability)?
    {
        return Err(stale_capability());
    }
    Ok((registration, physical))
}

fn stage_state_file_batch(preparation: &mut StateBatchPreparation) -> io::Result<()> {
    for member in &mut preparation.members {
        if member.stage.is_none() {
            member.stage = Some(
                match preparation.parent.create_recoverable_stage_with_old(
                    &member.name,
                    member.old,
                    Some(preparation.id),
                ) {
                    FileCreateOutcome::Created(staged) => StateBatchStage::Writing(staged),
                    FileCreateOutcome::NoEffect(error) => return Err(error),
                    FileCreateOutcome::AppliedUnverified(obligation) => {
                        let error = copy_io_error(obligation.error());
                        member.stage = Some(StateBatchStage::Creating(obligation));
                        return Err(error);
                    }
                },
            );
        }
        let staged = match member.stage.take() {
            Some(StateBatchStage::Writing(mut staged)) => {
                if let Err(error) = staged.write_all(&member.contents) {
                    member.stage = Some(StateBatchStage::Writing(staged));
                    return Err(error);
                }
                staged
            }
            Some(StateBatchStage::Sealed(staged)) => {
                member.stage = Some(StateBatchStage::Sealed(staged));
                continue;
            }
            Some(StateBatchStage::Creating(_) | StateBatchStage::Discarding(_)) | None => {
                unreachable!("fresh State batch member has no cleanup transition")
            }
        };
        member.stage = Some(match staged.seal() {
            Ok(staged) => StateBatchStage::Sealed(staged),
            Err(failure) => {
                let error = copy_io_error(failure.error());
                member.stage = Some(StateBatchStage::Writing(failure.into_staged()));
                return Err(error);
            }
        });
    }
    Ok(())
}

fn cancel_state_file_batch(
    preparation: &mut StateBatchPreparation,
) -> io::Result<Vec<(ReplaceDestination, Vec<u8>)>> {
    for member in &mut preparation.members {
        loop {
            let Some(stage) = member.stage.take() else {
                break;
            };
            match stage {
                StateBatchStage::Creating(obligation) => match obligation.reconcile() {
                    FileCreateResolution::Created(staged) => {
                        member.stage = Some(StateBatchStage::Writing(staged));
                    }
                    FileCreateResolution::NoEffect(_) => break,
                    FileCreateResolution::Indeterminate(obligation) => {
                        let error = copy_io_error(obligation.error());
                        member.stage = Some(StateBatchStage::Creating(obligation));
                        return Err(error);
                    }
                },
                StateBatchStage::Writing(staged) => match staged.discard() {
                    StageDiscardOutcome::Discarded => break,
                    StageDiscardOutcome::AppliedUnverified(obligation) => {
                        let error = copy_io_error(obligation.error());
                        member.stage = Some(StateBatchStage::Discarding(obligation));
                        return Err(error);
                    }
                },
                StateBatchStage::Sealed(staged) => match staged.discard() {
                    StageDiscardOutcome::Discarded => break,
                    StageDiscardOutcome::AppliedUnverified(obligation) => {
                        let error = copy_io_error(obligation.error());
                        member.stage = Some(StateBatchStage::Discarding(obligation));
                        return Err(error);
                    }
                },
                StateBatchStage::Discarding(obligation) => match obligation.reconcile() {
                    StageDiscardResolution::Discarded => break,
                    StageDiscardResolution::Indeterminate(obligation) => {
                        let error = copy_io_error(obligation.error());
                        member.stage = Some(StateBatchStage::Discarding(obligation));
                        return Err(error);
                    }
                },
            }
        }
    }
    let authority = preparation.parent.authority()?;
    let operation = authority.enter()?;
    preparation.parent.validate(&operation)?;
    let mut state = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
    if state.state_batch != Some(preparation.id)
        || state.recovery.records().next().is_some()
        || state.recovery.has_live_successor()
    {
        return Err(stale_capability());
    }
    state.state_batch.take();
    preparation.armed = false;
    Ok(preparation
        .members
        .iter_mut()
        .map(|member| {
            (
                member
                    .destination
                    .take()
                    .expect("cancelled State batch retains every destination"),
                std::mem::take(&mut member.contents),
            )
        })
        .collect())
}

#[expect(
    clippy::result_large_err,
    reason = "the failure must return every move-only batch and successor owner"
)]
fn prepare_state_file_batch(
    mut preparation: StateBatchPreparation,
    mut successor: Option<SuccessorOwner>,
) -> Result<StateBatchReplay, (io::Error, StateBatchPreparation, Option<SuccessorOwner>)> {
    let result = (|| -> io::Result<StateBatchReplay> {
        let authority = preparation.parent.authority()?;
        let operation = authority.enter()?;
        preparation.parent.validate(&operation)?;
        let mut observations = Vec::with_capacity(preparation.members.len());
        for member in &preparation.members {
            let destination = member
                .destination
                .as_ref()
                .expect("armed State batch retains every destination");
            let (parent, name, old) = validate_state_replace_destination(destination, &operation)?;
            if parent.inner.identity != preparation.parent.inner.identity
                || name != member.name
                || old != member.old
            {
                return Err(stale_capability());
            }
            let Some(StateBatchStage::Sealed(staged)) = member.stage.as_ref() else {
                return Err(stale_capability());
            };
            staged.file.validate(&operation)?;
            staged
                .file
                .validate_revision_in(&operation, &staged.revision)?;
            observations.push(observe_state_batch_stage(&authority, staged)?);
        }
        let mut state = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if state.phase != AUTHORITY_LIVE
            || state.state_batch != Some(preparation.id)
            || state.recovery.records().count() != preparation.members.len()
            || state.outstanding_effects < preparation.members.len()
        {
            return Err(stale_capability());
        }
        let mut planned = Vec::with_capacity(preparation.members.len());
        let mut targets = Vec::with_capacity(preparation.members.len());
        for (index, member) in preparation.members.iter().enumerate() {
            let Some(StateBatchStage::Sealed(staged)) = member.stage.as_ref() else {
                return Err(stale_capability());
            };
            let (registration, physical) =
                validate_state_batch_stage(&state, &authority, &preparation, member, staged)?;
            if planned
                .iter()
                .any(|(candidate, _, _)| *candidate == registration)
            {
                return Err(stale_capability());
            }
            let proof = observations[index].0;
            let mut desired = physical;
            desired.phase = RecoveryPhase::RemoveCommitted;
            desired.new = Some(proof);
            targets.push((member.name.clone(), proof));
            planned.push((registration, desired, proof));
        }
        let mut ordered = planned.iter().collect::<Vec<_>>();
        ordered.sort_by_key(|(registration, _, _)| registration.operation_id);
        let registrations = ordered
            .iter()
            .map(|(registration, _, _)| *registration)
            .collect::<Vec<_>>();
        let expected = ordered
            .iter()
            .map(|(registration, record, _)| (*registration, record.clone()))
            .collect::<Vec<_>>();
        if let Some(owner) = successor.as_ref() {
            state.recovery.validate_live_successor(owner)?;
        } else {
            let mut manifest = Vec::with_capacity(3 + registrations.len() * 56);
            manifest.extend_from_slice(&preparation.request.owner_schema.to_le_bytes());
            manifest.push(u8::try_from(registrations.len()).expect("State batch count is bounded"));
            for (_, record, proof) in &ordered {
                manifest.extend_from_slice(&record.operation_id);
                manifest.extend_from_slice(&proof.size.to_le_bytes());
                manifest.extend_from_slice(&proof.sha256);
            }
            let candidate = successor::SuccessorRecord {
                owner_class: successor::SuccessorOwnerClass::State,
                owner_schema: preparation.request.owner_schema,
                owner_id: preparation.request.owner_id.clone(),
                transfer_id: [0; 16],
                old_payload: None,
                new_payload: Some(manifest),
                acknowledgements: Vec::new(),
            };
            match state
                .recovery
                .create_successor(&authority.lease, candidate, &registrations)
            {
                Ok(owner) => successor = Some(owner),
                Err((error, owner)) => {
                    successor = owner;
                    return Err(error);
                }
            }
        }
        let descriptor = state
            .recovery
            .state_successor()?
            .ok_or_else(stale_capability)?;
        if descriptor.owner_schema != preparation.request.owner_schema
            || descriptor.owner_id != preparation.request.owner_id
            || descriptor.recoveries != expected
        {
            return Err(stale_capability());
        }
        for (index, member) in preparation.members.iter().enumerate() {
            let StateBatchStage::Sealed(staged) = member
                .stage
                .as_ref()
                .expect("prevalidated State batch member is sealed")
            else {
                unreachable!("prevalidated State batch member is sealed")
            };
            let (registration, desired, _) = &planned[index];
            let mut physical = desired.clone();
            physical.phase = RecoveryPhase::StagePrepared;
            physical.new = None;
            if validate_state_batch_stage(&state, &authority, &preparation, member, staged)?
                != (*registration, physical)
                || platform::file_identity(&staged.file.handle)? != staged.file.identity
                || platform::file_receipt_fields(&staged.file.handle)? != observations[index].1
                || platform::file_binding_state(
                    &preparation.parent.inner.handle,
                    staged.file.name.as_os_str(),
                    staged.file.identity,
                )? != platform::BindingState::Exact
            {
                return Err(stale_capability());
            }
        }
        preparation.parent.validate_for_authority(&authority)?;
        let journal = state.recovery.take_for_replay()?;
        let mut carriers = Vec::with_capacity(preparation.members.len());
        let mut cleanup = Vec::with_capacity(preparation.members.len());
        for (index, member) in preparation.members.iter_mut().enumerate() {
            let StateBatchStage::Sealed(staged) = member
                .stage
                .take()
                .expect("prevalidated State batch member is sealed")
            else {
                unreachable!("prevalidated State batch member is sealed")
            };
            let SealedStagedFile {
                file,
                mut token,
                revision: _,
            } = staged;
            cleanup.push(
                state
                    .stages
                    .remove(&token.id)
                    .expect("prevalidated State stage remains registered")
                    .cleanup,
            );
            token.armed = false;
            carriers.push(LiveStateCarrier {
                registration: planned[index].0,
                handle: file.handle,
                identity: file.identity,
                receipt: observations[index].1,
                proof: planned[index].2,
            });
            drop(token);
        }
        state.outstanding_effects -= preparation.members.len();
        drop(state);
        drop(cleanup);
        let successor = successor
            .take()
            .expect("durable State batch successor retains its exact owner");
        preparation.armed = false;
        for member in &mut preparation.members {
            drop(member.destination.take());
        }
        let replay = recovery_runtime::RecoveryReplay::from_live_state_successor(
            journal, successor, expected, carriers,
        );
        Ok(StateBatchReplay {
            replay,
            id: preparation.id,
            parent: preparation.parent.clone(),
            targets,
        })
    })();
    result.map_err(|error| (error, preparation, successor))
}

fn replay_state_file_batch(
    replay: StateBatchReplay,
) -> Result<StateBatchFinalization, (io::Error, StateBatchReplay)> {
    let authority = match replay.parent.authority() {
        Ok(authority) => authority,
        Err(error) => return Err((error, replay)),
    };
    let marker_valid = authority
        .operations
        .lock()
        .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))
        .is_ok_and(|state| state.state_batch == Some(replay.id));
    if !marker_valid {
        return Err((stale_capability(), replay));
    }
    match replay
        .replay
        .resume_state_successor(&authority.root, &authority.lease)
    {
        Ok((journal, orphans)) => Ok(StateBatchFinalization {
            journal: Some(Box::new(journal)),
            orphans: Some(orphans),
            id: replay.id,
            parent: replay.parent,
            targets: replay.targets,
        }),
        Err((error, retained)) => Err((
            error,
            StateBatchReplay {
                replay: retained,
                id: replay.id,
                parent: replay.parent,
                targets: replay.targets,
            },
        )),
    }
}

fn finalize_state_file_batch(
    mut finalization: StateBatchFinalization,
) -> Result<Vec<FileCapability>, (io::Error, StateBatchFinalization)> {
    let result = (|| -> io::Result<Vec<FileCapability>> {
        let authority = finalization.parent.authority()?;
        platform::validate_lease(&authority.lease)?;
        platform::validate_root(&authority.root)?;
        finalization.parent.validate_for_authority(&authority)?;
        let directory_stamp = platform::directory_revision(&finalization.parent.inner.handle)?;
        let listing = platform::entries(
            &finalization.parent.inner.handle,
            MAX_DIRECTORY_LIST_ENTRIES,
        )?;
        if !listing.complete {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "settled State batch scan exceeded its bound",
            ));
        }
        let mut files = Vec::with_capacity(finalization.targets.len());
        let mut receipts = Vec::with_capacity(finalization.targets.len());
        for (name, expected) in &finalization.targets {
            let mut matches = listing
                .entries
                .iter()
                .filter(|(candidate, _)| leaf_names_equivalent(candidate, name.as_os_str()));
            let Some((actual, EntryKind::File)) = matches.next() else {
                return Err(identity_changed(
                    "settled State batch destination is absent",
                ));
            };
            if actual.as_os_str() != name.as_os_str() || matches.next().is_some() {
                return Err(identity_changed(
                    "settled State batch destination acquired a portable alias",
                ));
            }
            let handle = platform::open_file(&finalization.parent.inner.handle, name.as_os_str())?;
            let identity = platform::file_identity(&handle)?;
            let proof = recovery_runtime::prove_file(
                &finalization.parent.inner.handle,
                name.as_os_str(),
                &handle,
                identity,
            )?;
            let receipt = platform::file_receipt_fields(&handle)?;
            if proof != *expected {
                return Err(identity_changed(
                    "settled State batch destination changed before admission",
                ));
            }
            files.push(FileCapability::new(
                handle,
                identity,
                finalization.parent.clone(),
                name.clone(),
                Arc::downgrade(&authority),
            ));
            receipts.push(receipt);
        }
        if platform::directory_revision(&finalization.parent.inner.handle)? != directory_stamp {
            return Err(identity_changed(
                "settled State batch changed during admission",
            ));
        }
        finalization.parent.validate_for_authority(&authority)?;
        let mut operations = authority
            .operations
            .lock()
            .map_err(|_| io::Error::other("filesystem capability operation lock was poisoned"))?;
        if operations.phase != AUTHORITY_LIVE
            || platform::directory_revision(&finalization.parent.inner.handle)? != directory_stamp
            || operations.state_batch != Some(finalization.id)
        {
            return Err(stale_capability());
        }
        for (index, file) in files.iter().enumerate() {
            if platform::file_identity(&file.handle)? != file.identity
                || platform::file_receipt_fields(&file.handle)? != receipts[index]
                || platform::file_binding_state(
                    &finalization.parent.inner.handle,
                    file.name.as_os_str(),
                    file.identity,
                )? != platform::BindingState::Exact
            {
                return Err(identity_changed(
                    "settled State batch changed before journal restoration",
                ));
            }
        }
        let journal = *finalization
            .journal
            .take()
            .expect("State batch finalization retains recovery journal");
        let orphans = finalization
            .orphans
            .take()
            .expect("State batch finalization retains recovery orphans");
        operations.recovery.restore_after_replay(journal);
        operations.recovery_orphans = orphans;
        assert_eq!(operations.state_batch.take(), Some(finalization.id));
        Ok(files)
    })();
    result.map_err(|error| (error, finalization))
}

fn retain_state_file_batch(error: io::Error, state: StateFileBatchState) -> StateFileBatchOutcome {
    StateFileBatchOutcome::AppliedUnverified(StateFileBatchObligation {
        error,
        state: Some(Box::new(state)),
    })
}

fn settle_state_file_batch(mut state: StateFileBatchState) -> StateFileBatchOutcome {
    loop {
        state = match state {
            StateFileBatchState::Preparing(mut preparation) => {
                if let Err(cause) = stage_state_file_batch(&mut preparation) {
                    StateFileBatchState::Rollback { cause, preparation }
                } else {
                    match prepare_state_file_batch(preparation, None) {
                        Ok(replay) => StateFileBatchState::Replaying(replay),
                        Err((error, preparation, Some(successor))) => {
                            return retain_state_file_batch(
                                error,
                                StateFileBatchState::Forward {
                                    preparation,
                                    successor,
                                },
                            );
                        }
                        Err((cause, preparation, None)) => {
                            StateFileBatchState::Rollback { cause, preparation }
                        }
                    }
                }
            }
            StateFileBatchState::Rollback {
                cause,
                mut preparation,
            } => match cancel_state_file_batch(&mut preparation) {
                Ok(replacements) => {
                    return StateFileBatchOutcome::NoEffect {
                        error: cause,
                        replacements,
                    };
                }
                Err(error) => {
                    return retain_state_file_batch(
                        error,
                        StateFileBatchState::Rollback { cause, preparation },
                    );
                }
            },
            StateFileBatchState::Forward {
                preparation,
                successor,
            } => match prepare_state_file_batch(preparation, Some(successor)) {
                Ok(replay) => StateFileBatchState::Replaying(replay),
                Err((error, preparation, successor)) => {
                    return retain_state_file_batch(
                        error,
                        StateFileBatchState::Forward {
                            preparation,
                            successor: successor
                                .expect("forward State batch retains its successor"),
                        },
                    );
                }
            },
            StateFileBatchState::Replaying(replay) => match replay_state_file_batch(replay) {
                Ok(finalization) => StateFileBatchState::Finalizing(finalization),
                Err((error, replay)) => {
                    return retain_state_file_batch(error, StateFileBatchState::Replaying(replay));
                }
            },
            StateFileBatchState::Finalizing(finalization) => {
                return match finalize_state_file_batch(finalization) {
                    Ok(files) => StateFileBatchOutcome::Replaced(files),
                    Err((error, finalization)) => retain_state_file_batch(
                        error,
                        StateFileBatchState::Finalizing(finalization),
                    ),
                };
            }
        };
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
    match staged.promote_no_replace_internal(&source, parent, name, displaced.as_ref()) {
        FilePromotionOutcome::Applied(current) => {
            FileReplaceOutcome::Replaced { current, displaced }
        }
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
                state: Some(Box::new(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                })),
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
            destination: receipt.rebuild_after_restore(file),
        },
        FileRestoreOutcome::NoEffect { error, parked } => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error,
                state: Some(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                })),
            })
        }
        FileRestoreOutcome::AppliedUnverified(restore) => {
            FileReplaceOutcome::AppliedUnverified(FileReplaceObligation {
                error: io::Error::other("replacement rollback is not yet settled"),
                state: Some(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                })),
            })
        }
    }
}

fn settle_file_replace(
    state: FileReplaceObligationState,
) -> Result<FileReplaceResolution, Box<FileReplaceObligationState>> {
    match state {
        FileReplaceObligationState::Parking {
            park,
            staged,
            receipt,
        } => {
            let (parent, name) = {
                let request = park
                    .request
                    .as_ref()
                    .expect("park obligation retains request");
                (request.file.parent.clone(), request.file.name.clone())
            };
            match park.reconcile() {
                FileParkResolution::Parked(displaced) => {
                    replace_outcome_to_resolution(continue_file_replace(
                        staged,
                        &parent,
                        &name,
                        Some(displaced),
                        ReplaceDestination::Vacant {
                            parent: parent.clone(),
                            name: name.clone(),
                        },
                        Some(receipt),
                    ))
                }
                FileParkResolution::NoEffect(request) => Ok(FileReplaceResolution::NoEffect {
                    staged,
                    destination: ReplaceDestination::Existing(request),
                }),
                FileParkResolution::Preserved { file, .. } => Ok(FileReplaceResolution::NoEffect {
                    staged,
                    destination: ReplaceDestination::Preserved(file),
                }),
                FileParkResolution::Indeterminate(park) => {
                    Err(Box::new(FileReplaceObligationState::Parking {
                        park,
                        staged,
                        receipt,
                    }))
                }
            }
        }
        FileReplaceObligationState::Promoting {
            promotion,
            displaced,
            fallback,
            receipt,
        } => match (*promotion).reconcile() {
            FilePromotionResolution::Applied(current) => {
                Ok(FileReplaceResolution::Replaced { current, displaced })
            }
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
                Err(Box::new(FileReplaceObligationState::Promoting {
                    promotion,
                    displaced,
                    fallback,
                    receipt,
                }))
            }
        },
        FileReplaceObligationState::RestoreParked {
            parked,
            staged,
            receipt,
        } => match parked.restore() {
            FileRestoreOutcome::Restored(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: receipt.rebuild_after_restore(file),
            }),
            FileRestoreOutcome::NoEffect { parked, .. } => {
                Err(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                }))
            }
            FileRestoreOutcome::AppliedUnverified(restore) => {
                Err(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                }))
            }
        },
        FileReplaceObligationState::RestoreObligation {
            restore,
            staged,
            receipt,
        } => match restore.reconcile() {
            FileRestoreResolution::Restored(file) => Ok(FileReplaceResolution::NoEffect {
                staged,
                destination: receipt.rebuild_after_restore(file),
            }),
            FileRestoreResolution::NoEffect(parked) => {
                Err(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt,
                }))
            }
            FileRestoreResolution::Indeterminate(restore) => {
                Err(Box::new(FileReplaceObligationState::RestoreObligation {
                    restore,
                    staged,
                    receipt,
                }))
            }
        },
    }
}

fn replace_outcome_to_resolution(
    outcome: FileReplaceOutcome,
) -> Result<FileReplaceResolution, Box<FileReplaceObligationState>> {
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
    /// Completes this writer; sealing the stage is the durability boundary.
    pub fn finish(self) -> io::Result<()> {
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
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::File(parked.token.id))
            else {
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
            let Some(permit) = permit_by_park.get(&DrainRecoveryParkId::Directory(parked.token.id))
            else {
                self.directories.push(parked);
                continue;
            };
            match decision {
                DrainRecoveryDecision::Remove => match parked.remove_empty_with_recovery(permit) {
                    DirectoryRemovalOutcome::Removed => {}
                    DirectoryRemovalOutcome::NoEffect { parked, .. } => {
                        self.directories.push(parked);
                    }
                    DirectoryRemovalOutcome::AppliedUnverified(obligation) => {
                        self.directory_removals.push(obligation);
                    }
                },
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

    fn transfer_preserved_directory_creates_to_reset(&mut self, permits: &[DrainRecoveryPermit]) {
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
        formatter
            .debug_struct("RootSession")
            .finish_non_exhaustive()
    }
}

impl RootSession {
    pub fn acquire(path: &Path) -> RootSessionAcquireOutcome {
        let process_image = match capture_process_image_ancestry() {
            Ok(process_image) => process_image,
            Err(error) => {
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::ProcessImage(error));
            }
        };
        match platform::open_or_create_root(path) {
            Ok(construction) => try_acquire_lease_and_finish_root(construction, process_image),
            Err(error) => {
                let (error, construction) = error.into_parts();
                match construction {
                    Some(construction) => {
                        RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                            error: RootSessionError::Create(error),
                            construction: Some(construction),
                            lease: None,
                            acquired: None,
                            process_image: Some(process_image),
                        })
                    }
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
                return RootSessionAcquireOutcome::NoEffect(RootSessionError::ProcessImage(error));
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
        let Some(guard) = admitted.inner.directory.inner.absolute_ancestry.as_ref() else {
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
        let start = self.authority.begin_terminal_drain(false);
        match start {
            Ok(()) => RootRevokeDrain {
                session: Some(self),
            }
            .try_settle(),
            Err(error) => RootRevokeOutcome::Refused(RootRevokeStartFailure::new(self, error)),
        }
    }

    pub fn begin_reset(self) -> ResetStartOutcome {
        if let Err(error) = self
            .authority
            .validate_retained_process_image_outside_root()
        {
            return ResetStartOutcome::Refused(ResetStartFailure::new(self, error));
        }
        let start = self.authority.begin_terminal_drain(true);
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
                    acquired: None,
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
                acquired: None,
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
                    acquired: None,
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
                    acquired: None,
                    process_image: Some(process_image),
                })
            } else {
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Lease(error))
            };
        }
        platform::LeaseAcquisitionOutcome::AppliedUnverified(lease) => {
            let error =
                RootSessionError::Lease(copy_io_error(platform::lease_acquisition_error(&lease)));
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error,
                construction: Some(construction),
                lease: Some(lease),
                acquired: None,
                process_image: Some(process_image),
            });
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
    let root = match platform::root_construction_guard(&construction) {
        Ok(root) => root,
        Err(error) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Create(error),
                construction: Some(construction),
                lease: None,
                acquired: Some(Box::new(AcquiredRoot {
                    lease,
                    replay: None,
                })),
                process_image: Some(process_image),
            });
        }
    };
    let recovery = match recovery_runtime::initialize_and_replay(root, &lease) {
        Ok(recovery) => recovery,
        Err((error, replay)) => {
            return RootSessionAcquireOutcome::AppliedUnverified(RootSessionAcquireObligation {
                error: RootSessionError::Recovery(error),
                construction: Some(construction),
                lease: None,
                acquired: Some(Box::new(AcquiredRoot { lease, replay })),
                process_image: Some(process_image),
            });
        }
    };
    finish_root_session_with_recovery(construction, identity, lease, process_image, recovery)
}

fn finish_root_session_with_recovery(
    construction: platform::RootConstruction,
    identity: platform::Identity,
    lease: platform::LeaseHandle,
    process_image: platform::ProcessImageAncestry,
    (recovery, recovery_orphans): (RecoveryJournal, Vec<RecoveryOrphan>),
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
                root_identity: identity,
                active: 0,
                outstanding_effects: 0,
                next_move_id: 1,
                moves: HashMap::new(),
                next_effect_owner_id: 1,
                effect_owner_handles: HashMap::new(),
                active_effect_owners: HashMap::new(),
                next_stage_id: 1,
                next_publication_attempt_id: 1,
                stages: HashMap::new(),
                next_stage_create_id: 1,
                stage_creations: HashMap::new(),
                next_directory_create_id: 1,
                directory_creations: HashMap::new(),
                next_file_park_id: 1,
                file_parks: HashMap::new(),
                next_directory_park_id: 1,
                directory_parks: HashMap::new(),
                next_transient_id: 1,
                transients: HashMap::new(),
                state_batch: None,
                recovery,
                recovery_orphans,
            }),
            #[cfg(test)]
            directory_open_pause: Mutex::new(None),
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
        if phase == AUTHORITY_LIVE && self.authority.begin_terminal_drain(false).is_err() {
            std::process::abort();
        }
        let state = match self.authority.operations.lock() {
            Ok(state) => state,
            Err(_) => std::process::abort(),
        };
        if state.active != 0
            || state.outstanding_effects != 0
            || !state.moves.is_empty()
            || !state.stages.is_empty()
            || !state.stage_creations.is_empty()
            || !state.directory_creations.is_empty()
            || !state.file_parks.is_empty()
            || !state.directory_parks.is_empty()
            || !state.transients.is_empty()
            || state.state_batch.is_some()
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
    Recovery { recovery: RootRevokeRecovery },
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
            RootRevokeOutcome::Recovery { recovery: self }
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
    Recovery { recovery: ResetDrainRecovery },
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
        let cancellation = session.authority.cancel_reset_pending_directory_creates();
        let revoke = RootRevokeDrain {
            session: Some(session),
        };
        match cancellation {
            Ok(()) => revoke.try_settle(),
            Err(error) => RootRevokeOutcome::Failed(RootRevokeDrainFailure::new(revoke, error)),
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
            ResetStartOutcome::Recovery { recovery: self }
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

    pub fn clear_root(self) -> RootClearOutcome {
        let result = (|| {
            let session = self.session.as_ref().expect("reset session is retained");
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
        let session = self.session.as_ref().expect("reset session is retained");
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
        || !Weak::ptr_eq(&source_parent.inner.authority, &destination.inner.authority)
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
    LeafName::new(format!("{prefix}{}", hex::encode(nonce)))
        .expect("generated filesystem leaf is valid")
}

fn hash_parked_file(file: &platform::FileCleanupHandle, size: u64) -> io::Result<[u8; 32]> {
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
) -> io::Result<(u64, platform::FileStamp)> {
    let receipt = observe_parked_revision(
        parent,
        park_name,
        parked,
        expected_content.revision.identity,
    )?;
    if receipt.0 != expected_content.revision.size
        || !platform::file_content_stamp_matches(receipt.1, expected_content.revision.stamp)
        || hash_parked_file(parked, expected_content.revision.size)? != expected_content.sha256
        || observe_parked_revision(
            parent,
            park_name,
            parked,
            expected_content.revision.identity,
        )
        .ok()
            != Some(receipt)
    {
        return Err(identity_changed(
            "parked file did not match its expected content receipt",
        ));
    }
    Ok(receipt)
}

fn verify_parked_revision(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    identity: platform::Identity,
    size: u64,
    stamp: platform::FileStamp,
) -> io::Result<()> {
    if observe_parked_revision(parent, park_name, parked, identity)? != (size, stamp) {
        return Err(identity_changed("parked file revision changed"));
    }
    Ok(())
}

fn observe_parked_revision(
    parent: &Directory,
    park_name: &LeafName,
    parked: &platform::FileCleanupHandle,
    identity: platform::Identity,
) -> io::Result<(u64, platform::FileStamp)> {
    let receipt = platform::parked_file_receipt_fields(parked)?;
    if platform::file_binding_state(&parent.inner.handle, park_name.as_os_str(), identity)?
        != platform::BindingState::Exact
    {
        return Err(identity_changed("parked file binding changed"));
    }
    Ok(receipt)
}

fn prove_restored_file<F>(
    request: &FileParkRequest,
    record: &FileParkSettlementRecord,
    operation: &CapabilityOperation,
    after_hash: F,
) -> io::Result<RestoredFileProof>
where
    F: FnOnce(),
{
    request.file.validate(operation)?;
    if request.file.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        || request.expected.revision.authority.as_ptr() != Arc::as_ptr(&operation.authority)
        || request.file.identity != record.identity
        || request.expected.revision.identity != record.identity
        || platform::parked_file_identity(&record.cleanup)? != record.identity
    {
        return Err(identity_changed(
            "restored file authority changed before content proof",
        ));
    }
    let before = observe_parked_revision(
        &record.parent,
        &record.original_name,
        &record.cleanup,
        record.identity,
    )?;
    let digest = hash_parked_file(&record.cleanup, before.0)?;
    after_hash();
    request.file.validate(operation)?;
    if platform::parked_file_identity(&record.cleanup)? != record.identity {
        return Err(identity_changed(
            "restored file authority changed during content proof",
        ));
    }
    let after = observe_parked_revision(
        &record.parent,
        &record.original_name,
        &record.cleanup,
        record.identity,
    )?;
    if after != before {
        return Err(identity_changed(
            "restored file changed while its content was being proven",
        ));
    }
    if before.0 != request.expected.revision.size || digest != request.expected.sha256 {
        return Ok(RestoredFileProof::Preserved);
    }
    Ok(RestoredFileProof::Current(FileRevision {
        authority: request.file.authority.clone(),
        identity: record.identity,
        size: before.0,
        stamp: before.1,
    }))
}

fn finish_new_file_park(
    mut request: FileParkRequest,
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
                #[cfg(test)]
                restored_proof_pause: None,
            });
        }
    };
    match verify_parked_file(
        &parent,
        &park_name,
        &guard.record().cleanup,
        &request.expected,
    ) {
        Ok((size, stamp)) => {
            let record = guard.record_mut();
            record.size = size;
            record.stamp = stamp;
            record.expected_digest = None;
            if parent.validate(operation).is_ok() {
                record.phase = FileParkRegistryPhase::Live;
                drop(guard);
                FileParkOutcome::Parked(ParkedFile {
                    parent,
                    original_name,
                    park_name,
                    identity,
                    size,
                    stamp,
                    verified: true,
                    token,
                    authority: request.file.authority.clone(),
                })
            } else {
                drop(guard);
                FileParkOutcome::AppliedUnverified(FileParkObligation {
                    error: identity_changed("file park lost its authority chain"),
                    request: Some(request),
                    token,
                    park_name,
                    phase: FileParkPhase::Parking,
                    digest_verified: true,
                    #[cfg(test)]
                    restored_proof_pause: None,
                })
            }
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
                Ok(_) => match prove_restored_file(&request, guard.record(), operation, || {}) {
                    Ok(RestoredFileProof::Current(revision)) => {
                        request.expected.revision = revision;
                        guard.disarm(&mut token, operation);
                        FileParkOutcome::NoEffect { error, request }
                    }
                    Ok(RestoredFileProof::Preserved) => {
                        let FileParkRequest { file, expected: _ } = request;
                        guard.disarm(&mut token, operation);
                        FileParkOutcome::Preserved {
                            error: identity_changed(
                                "restored file content no longer matches the park request",
                            ),
                            file,
                        }
                    }
                    Err(proof) => {
                        drop(guard);
                        FileParkOutcome::AppliedUnverified(FileParkObligation {
                            error: proof,
                            request: Some(request),
                            token,
                            park_name,
                            phase: FileParkPhase::RestoringRejectedReceipt,
                            digest_verified: false,
                            #[cfg(test)]
                            restored_proof_pause: None,
                        })
                    }
                },
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
                        #[cfg(test)]
                        restored_proof_pause: None,
                    })
                }
            }
        }
    }
}

fn settle_file_park(mut obligation: FileParkObligation, force_restore: bool) -> FileParkResolution {
    #[cfg(test)]
    let restored_proof_pause = obligation.restored_proof_pause.take();
    let request = obligation
        .request
        .as_ref()
        .expect("park obligation retains request");
    let authority = match request.file.parent.authority() {
        Ok(authority) => authority,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    let operation = match authority.enter() {
        Ok(operation) => operation,
        Err(_) => return FileParkResolution::Indeterminate(obligation),
    };
    if request.file.parent.validate(&operation).is_err() {
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
            let proof = prove_restored_file(request, guard.record(), &operation, || {
                #[cfg(test)]
                if let Some(pause) = restored_proof_pause.as_ref() {
                    pause.hashed.wait();
                    pause.resume.wait();
                }
            });
            match proof {
                Ok(RestoredFileProof::Current(revision)) => {
                    obligation
                        .request
                        .as_mut()
                        .expect("park obligation retains request")
                        .expected
                        .revision = revision;
                    guard.disarm(&mut obligation.token, &operation);
                    FileParkResolution::NoEffect(obligation.request.take().expect("park request"))
                }
                Ok(RestoredFileProof::Preserved) => {
                    let FileParkRequest { file, expected: _ } =
                        obligation.request.take().expect("park request");
                    guard.disarm(&mut obligation.token, &operation);
                    FileParkResolution::Preserved {
                        error: identity_changed(
                            "restored file content no longer matches the park request",
                        ),
                        file,
                    }
                }
                Err(error) => {
                    obligation.error = error;
                    drop(guard);
                    FileParkResolution::Indeterminate(obligation)
                }
            }
        }
        (Ok(platform::BindingState::Absent), Ok(platform::BindingState::Exact)) => {
            if request
                .validate_authority_after_namespace_change(&operation)
                .is_err()
            {
                drop(guard);
                return FileParkResolution::Indeterminate(obligation);
            }
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
                    Ok(_) => {
                        let proof =
                            prove_restored_file(request, guard.record(), &operation, || {
                                #[cfg(test)]
                                if let Some(pause) = restored_proof_pause.as_ref() {
                                    pause.hashed.wait();
                                    pause.resume.wait();
                                }
                            });
                        match proof {
                            Ok(RestoredFileProof::Current(revision)) => {
                                obligation
                                    .request
                                    .as_mut()
                                    .expect("park obligation retains request")
                                    .expected
                                    .revision = revision;
                                guard.disarm(&mut obligation.token, &operation);
                                FileParkResolution::NoEffect(
                                    obligation.request.take().expect("park request"),
                                )
                            }
                            Ok(RestoredFileProof::Preserved) => {
                                let FileParkRequest { file, expected: _ } =
                                    obligation.request.take().expect("park request");
                                guard.disarm(&mut obligation.token, &operation);
                                FileParkResolution::Preserved {
                                    error: identity_changed(
                                        "restored file content no longer matches the park request",
                                    ),
                                    file,
                                }
                            }
                            Err(error) => {
                                obligation.error = error;
                                drop(guard);
                                FileParkResolution::Indeterminate(obligation)
                            }
                        }
                    }
                    Err(error) => {
                        obligation.error = error;
                        drop(guard);
                        FileParkResolution::Indeterminate(obligation)
                    }
                };
            }
            if !obligation.digest_verified {
                let (size, stamp) = match verify_parked_file(
                    &request.file.parent,
                    &obligation.park_name,
                    &guard.record().cleanup,
                    &request.expected,
                ) {
                    Ok(receipt) => receipt,
                    Err(error) => {
                        obligation.error = error;
                        obligation.phase = FileParkPhase::RestoringRejectedReceipt;
                        drop(guard);
                        return settle_file_park(obligation, true);
                    }
                };
                obligation.digest_verified = true;
                let record = guard.record_mut();
                record.size = size;
                record.stamp = stamp;
                record.expected_digest = None;
            }
            let request = obligation.request.take().expect("park request");
            let record = guard.record();
            if verify_parked_revision(
                &request.file.parent,
                &obligation.park_name,
                &record.cleanup,
                record.identity,
                record.size,
                record.stamp,
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
            let record = guard.record_mut();
            record.phase = FileParkRegistryPhase::Live;
            let size = record.size;
            let stamp = record.stamp;
            drop(guard);
            FileParkResolution::Parked(ParkedFile {
                parent: request.file.parent.clone(),
                original_name: request.file.name.clone(),
                park_name: obligation.park_name,
                identity: request.file.identity,
                size,
                stamp,
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
                let initial_error = format!("{:?}", obligation.error());
                match obligation.reconcile() {
                    RootSessionAcquireOutcome::Acquired(session) => session,
                    RootSessionAcquireOutcome::NoEffect(error) => {
                        panic!("root acquisition reconciliation had no effect: {error}")
                    }
                    RootSessionAcquireOutcome::AppliedUnverified(obligation) => {
                        let error = format!("{:?}", obligation.error());
                        let obligation = obligation
                            .cleanup()
                            .expect_err("acquired recovery owner refuses test cleanup");
                        obligation
                            .acknowledge_preserved()
                            .expect("acknowledge failed test acquisition");
                        panic!(
                            "root acquisition remained indeterminate: initial {initial_error}; retry {error}"
                        )
                    }
                }
            }
        }
    }

    #[test]
    fn root_session_creates_a_missing_nested_root() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root_path = temporary.path().join("missing").join("nested-root");

        let session = acquire_test_root(&root_path);

        assert!(root_path.is_dir());
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn cloned_root_capabilities_serialize_multipage_enumeration() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let expected = (0..384)
            .map(|index| format!("entry-{index:03}-{}", "x".repeat(180)))
            .collect::<std::collections::BTreeSet<_>>();
        for name in &expected {
            std::fs::write(temporary.path().join(name), b"").expect("directory entry");
        }
        let session = acquire_test_root(temporary.path());
        let first = session.root().expect("first root capability");
        let second = session.root().expect("second root capability");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let enumerate = |root: Directory, barrier: std::sync::Arc<std::sync::Barrier>| {
            std::thread::spawn(move || {
                barrier.wait();
                let mut observed = None;
                for _ in 0..8 {
                    let listing = root.entries(512).expect("complete directory listing");
                    assert_eq!(listing.state(), DirectoryListingState::Complete);
                    let names = listing
                        .entries()
                        .iter()
                        .filter_map(DirectoryEntry::utf8_name)
                        .map(str::to_owned)
                        .collect::<std::collections::BTreeSet<_>>();
                    if let Some(previous) = &observed {
                        assert_eq!(previous, &names);
                    } else {
                        observed = Some(names);
                    }
                }
                observed.expect("enumerated names")
            })
        };
        let first = enumerate(first, std::sync::Arc::clone(&barrier));
        let second = enumerate(second, barrier);
        let first = first.join().expect("first enumeration worker");
        let second = second.join().expect("second enumeration worker");

        assert_eq!(first, second);
        assert!(expected.is_subset(&first));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_projection_tolerates_one_harmless_sibling_change() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root_path = temporary.path().join("projection-root");
        std::fs::create_dir(&root_path).expect("projection root");
        let session = acquire_test_root(&root_path);
        let root = session.root().expect("root capability");
        let target_name = root_path
            .file_name()
            .expect("projection root name")
            .to_os_string();
        let sibling = temporary.path().join("harmless-sibling");
        let mutations = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let hook_mutations = std::rc::Rc::clone(&mutations);
        let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
            if name == target_name && hook_mutations.get() == 0 {
                std::fs::create_dir(&sibling).expect("create harmless sibling");
                hook_mutations.set(1);
            }
        }));

        root.validate_absolute_projection(&root_path)
            .expect("one sibling change does not invalidate the target");

        assert_eq!(mutations.get(), 1);
        drop(hook);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_projection_tolerates_sustained_sibling_churn() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root_path = temporary.path().join("projection-root");
        std::fs::create_dir(&root_path).expect("projection root");
        let session = acquire_test_root(&root_path);
        let root = session.root().expect("root capability");
        let target_name = root_path
            .file_name()
            .expect("projection root name")
            .to_os_string();
        let sibling_parent = temporary.path().to_path_buf();
        let mutations = std::rc::Rc::new(std::cell::Cell::new(0_usize));
        let hook_mutations = std::rc::Rc::clone(&mutations);
        let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
            if name != target_name {
                return;
            }
            for mutation in 0..64 {
                let sibling = sibling_parent.join(format!("unstable-sibling-{mutation}"));
                std::fs::create_dir(&sibling).expect("create unstable sibling");
                std::fs::remove_dir(&sibling).expect("remove unstable sibling");
                hook_mutations.set(hook_mutations.get() + 1);
            }
        }));

        root.validate_absolute_projection(&root_path)
            .expect("sustained sibling churn does not invalidate the target");

        assert_eq!(mutations.get(), 64);
        drop(hook);
        root.validate_absolute_projection(&root_path)
            .expect("unchanged target remains valid after churn");
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn absolute_projection_refuses_case_alias_introduced_during_proof() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root_path = temporary.path().join("projection-root");
        let alias_path = temporary.path().join("Projection-Root");
        std::fs::create_dir(&root_path).expect("projection root");
        let session = acquire_test_root(&root_path);
        let root = session.root().expect("root capability");
        let target_name = root_path
            .file_name()
            .expect("projection root name")
            .to_os_string();
        let hook_root = root_path.clone();
        let hook_alias = alias_path.clone();
        let renamed = std::rc::Rc::new(std::cell::Cell::new(false));
        let hook_renamed = std::rc::Rc::clone(&renamed);
        let hook = platform::install_exact_directory_binding_test_hook(Box::new(move |name| {
            if name == target_name && !hook_renamed.replace(true) {
                std::fs::rename(&hook_root, &hook_alias).expect("introduce case alias");
            }
        }));

        let error = root
            .validate_absolute_projection(&root_path)
            .expect_err("case alias must invalidate exact projection");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(renamed.get());
        drop(hook);
        std::fs::rename(&alias_path, &root_path).expect("restore exact root spelling");
        root.validate_absolute_projection(&root_path)
            .expect("restored exact target remains valid");
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn owned_symlink_proves_a_parent_relative_target_beneath_its_root() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("bin")).expect("bin directory");
        std::fs::create_dir(temporary.path().join("lib")).expect("lib directory");
        std::fs::write(temporary.path().join("lib/runtime.bin"), b"runtime")
            .expect("runtime target");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let bin = LeafName::new("bin").expect("bin leaf");
        let link = LeafName::new("runtime").expect("link leaf");

        root.create_owned_symlink_beneath(
            std::slice::from_ref(&bin),
            &link,
            OsStr::new("../lib/runtime.bin"),
        )
        .expect("contained owned link");

        let bin_directory = root.open_directory(&bin).expect("bin capability");
        assert_eq!(
            bin_directory.read_symlink(&link).expect("link target"),
            OsStr::new("../lib/runtime.bin")
        );
        drop((bin_directory, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn owned_symlink_rejects_escape_missing_and_link_targets() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("bin")).expect("bin directory");
        std::fs::create_dir(temporary.path().join("lib")).expect("lib directory");
        std::fs::write(temporary.path().join("lib/runtime.bin"), b"runtime")
            .expect("runtime target");
        symlink("runtime.bin", temporary.path().join("lib/runtime.alias"))
            .expect("target link fixture");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let bin = LeafName::new("bin").expect("bin leaf");

        for (name, target, kind) in [
            ("escape", "../../outside", io::ErrorKind::InvalidInput),
            ("missing", "../lib/missing", io::ErrorKind::NotFound),
            (
                "link-target",
                "../lib/runtime.alias",
                io::ErrorKind::InvalidInput,
            ),
        ] {
            assert_eq!(
                root.create_owned_symlink_beneath(
                    std::slice::from_ref(&bin),
                    &LeafName::new(name).expect("link leaf"),
                    OsStr::new(target),
                )
                .expect_err("unsafe target must be rejected")
                .kind(),
                kind
            );
            assert!(!temporary.path().join("bin").join(name).exists());
        }
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
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
    fn admitted_absolute_directory_supports_staged_file_effects() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let app_root = temporary.path().join("app");
        let library = temporary.path().join("library");
        std::fs::create_dir(&app_root).expect("create app root");
        std::fs::create_dir(&library).expect("create library root");
        let session = acquire_test_root(&app_root);
        let admitted = session
            .admit_absolute_directory(&library)
            .expect("admit library");

        let staged = test_sealed_stage(&admitted, b"managed payload");
        let name = LeafName::new("published.bin").expect("published leaf");
        let published =
            require_test_promotion(staged.promote_no_replace(&admitted, &admitted, &name));

        drop(published);
        assert_eq!(
            std::fs::read(library.join("published.bin")).expect("published payload"),
            b"managed payload"
        );
        drop(admitted);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn sealed_stage_promotes_across_directories() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("source")).expect("source directory");
        std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let source = root
            .open_directory(&LeafName::new("source").expect("source leaf"))
            .expect("source capability");
        let destination = root
            .open_directory(&LeafName::new("destination").expect("destination leaf"))
            .expect("destination capability");

        let staged = test_sealed_stage(&source, b"managed payload");
        let name = LeafName::new("published.bin").expect("published leaf");
        let published =
            require_test_promotion(staged.promote_no_replace(&source, &destination, &name));

        assert_eq!(
            published.read_bounded(32).expect("published payload"),
            b"managed payload"
        );
        drop((root, source, destination, published));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn sealed_stage_collision_is_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("published.bin"), b"existing payload")
            .expect("collision destination");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let staged = test_sealed_stage(&root, b"new payload");
        let name = LeafName::new("published.bin").expect("published leaf");

        let staged = match staged.promote_no_replace(&root, &root, &name) {
            FilePromotionOutcome::NoEffect { staged, .. } => staged,
            FilePromotionOutcome::Applied(_) => panic!("collision replaced its destination"),
            FilePromotionOutcome::AppliedUnverified(obligation) => {
                panic!("collision remained indeterminate: {}", obligation.error())
            }
        };

        assert_eq!(
            std::fs::read(temporary.path().join("published.bin")).expect("collision payload"),
            b"existing payload"
        );
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn sealed_stage_refuses_content_revision_drift() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let staged = test_sealed_stage(&root, b"sealed payload");
        staged
            .file
            .handle
            .set_len(1)
            .expect("mutate sealed stage length");
        let name = LeafName::new("published.bin").expect("published leaf");

        let staged = match staged.promote_no_replace(&root, &root, &name) {
            FilePromotionOutcome::NoEffect { staged, .. } => staged,
            FilePromotionOutcome::Applied(_) => panic!("changed sealed stage was published"),
            FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
                "changed sealed stage became indeterminate: {}",
                obligation.error()
            ),
        };

        assert!(!temporary.path().join("published.bin").exists());
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn sealed_stage_refuses_live_park_original_reservation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let original_path = temporary.path().join("reserved.bin");
        std::fs::write(&original_path, b"original payload").expect("original file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_file(&root, "reserved.bin", "reserved.park", b"original payload");
        assert!(!original_path.exists());
        let staged = test_sealed_stage(&root, b"replacement payload");
        let destination = LeafName::new("RESERVED.BIN").expect("portable alias");

        let staged = match staged.promote_no_replace(&root, &root, &destination) {
            FilePromotionOutcome::NoEffect { error, staged } => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                staged
            }
            FilePromotionOutcome::Applied(_) => panic!("stage occupied a live park reservation"),
            FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
                "reserved stage promotion became indeterminate: {}",
                obligation.error()
            ),
        };
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        let restored = match parked.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("park restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!("park restoration was indeterminate: {}", obligation.error())
            }
        };
        assert_eq!(
            std::fs::read(&original_path).expect("restored payload"),
            b"original payload"
        );

        drop((restored, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn file_park_checkout_roundtrip_preserves_learned_revision() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("record.bin"), b"payload").expect("record file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_file(&root, "record.bin", "record.park", b"payload");

        parked
            .validate_current()
            .expect("first checkout retained learned revision");
        parked
            .validate_current()
            .expect("second checkout restored learned revision");
        let restored = match parked.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("park restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!("park restoration was indeterminate: {}", obligation.error())
            }
        };

        drop((restored, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn hardlink_aliases_cannot_hold_independent_file_parks() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("first.bin"), b"payload").expect("first file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let first = park_test_file(&root, "first.bin", "first.park", b"payload");
        std::fs::hard_link(
            temporary.path().join("first.park"),
            temporary.path().join("second.bin"),
        )
        .expect("second hardlink");
        std::fs::remove_file(temporary.path().join("first.park"))
            .expect("leave the retained inode at one disjoint link");
        let second_file = root
            .open_file(&LeafName::new("second.bin").expect("second leaf"))
            .expect("second capability");
        let second_revision = second_file.revision().expect("second revision");
        let digest: [u8; 32] = Sha256::digest(b"payload").into();
        let request = second_file.park_request(ExpectedFileContent::new(second_revision, digest));

        let second_file = match root.park_file_as(
            request,
            LeafName::new("second.park").expect("second park leaf"),
        ) {
            FileParkOutcome::NoEffect { error, request } => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                request.file
            }
            FileParkOutcome::Parked(_) => panic!("hardlink alias acquired a second park owner"),
            FileParkOutcome::Preserved { error, .. } => {
                panic!("hardlink conflict changed content: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "hardlink conflict became indeterminate: {}",
                    obligation.error()
                )
            }
        };
        drop(second_file);
        std::fs::rename(
            temporary.path().join("second.bin"),
            temporary.path().join("first.park"),
        )
        .expect("restore first park topology");
        let restored = match first.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("first park restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "first park restoration was indeterminate: {}",
                    obligation.error()
                )
            }
        };

        drop((restored, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn file_move_handoff_occupies_only_its_exact_park_original() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
        std::fs::write(temporary.path().join("staged.bin"), b"new payload").expect("staged file");
        std::fs::write(temporary.path().join("unrelated.bin"), b"unrelated payload")
            .expect("unrelated file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
        let unrelated = park_test_file(
            &root,
            "unrelated.bin",
            "unrelated.park",
            b"unrelated payload",
        );
        let staged = root
            .open_file(&LeafName::new("staged.bin").expect("staged leaf"))
            .expect("staged capability");
        let target = LeafName::new("target.bin").expect("target leaf");

        let staged = match staged.move_no_replace(&root, &target) {
            FileMoveOutcome::NoEffect { error, file } => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
                file
            }
            FileMoveOutcome::Applied(_) => panic!("ordinary move bypassed the park reservation"),
            FileMoveOutcome::AppliedUnverified(obligation) => {
                panic!("reserved move became indeterminate: {}", obligation.error())
            }
        };
        let (current, parked) = match staged.move_no_replace_after_park(&root, &target, parked) {
            FileMoveAfterParkOutcome::Applied { current, displaced } => (current, displaced),
            FileMoveAfterParkOutcome::NoEffect { error, .. } => {
                panic!("exact park handoff had no effect: {error}")
            }
            FileMoveAfterParkOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "exact park handoff remained indeterminate: {}",
                    obligation.error()
                )
            }
        };
        assert_eq!(
            current.read_bounded(32).expect("current payload"),
            b"new payload"
        );
        parked
            .validate_current()
            .expect("settled move released its exact park link");
        unrelated
            .validate_current()
            .expect("exact handoff did not consume an unrelated park");
        assert!(matches!(parked.remove(), FileRemovalOutcome::Removed));
        let unrelated = match unrelated.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("unrelated restore had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "unrelated restore remained indeterminate: {}",
                    obligation.error()
                )
            }
        };

        drop((current, unrelated, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn file_move_handoff_link_survives_indeterminate_reconciliation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
        std::fs::write(temporary.path().join("staged.bin"), b"new payload").expect("staged file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
        let staged = root
            .open_file(&LeafName::new("staged.bin").expect("staged leaf"))
            .expect("staged capability");
        let target = LeafName::new("target.bin").expect("target leaf");
        let authority = root.authority().expect("root authority");
        let token = {
            let operation = authority.enter().expect("move reservation operation");
            MoveEffectToken::reserve(
                &authority,
                &operation,
                NamespaceLeaf {
                    parent: root.clone(),
                    name: staged.name.clone(),
                },
                NamespaceLeaf {
                    parent: root.clone(),
                    name: target.clone(),
                },
                None,
                Some(staged.identity),
                Some(&parked.token),
            )
            .expect("exact move handoff reservation")
        };
        std::fs::rename(
            temporary.path().join("staged.bin"),
            temporary.path().join("displaced-stage.bin"),
        )
        .expect("displace staged source");
        let obligation = FileMoveAfterParkObligation {
            movement: FileMoveObligation {
                error: io::Error::other("test move requires reconciliation"),
                file: Some(staged),
                destination: root.clone(),
                destination_name: target,
                reported_success: false,
                token,
            },
            displaced: parked,
        };
        let obligation = match obligation.reconcile() {
            FileMoveAfterParkResolution::Indeterminate(obligation) => obligation,
            _ => panic!("displaced source did not preserve the move obligation"),
        };
        assert_eq!(
            obligation
                .displaced
                .validate_current()
                .expect_err("indeterminate move retains its park link")
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        std::fs::rename(
            temporary.path().join("displaced-stage.bin"),
            temporary.path().join("staged.bin"),
        )
        .expect("restore staged source");
        let (staged, parked) = match obligation.reconcile() {
            FileMoveAfterParkResolution::NoEffect { source, displaced } => (source, displaced),
            FileMoveAfterParkResolution::Applied { .. } => {
                panic!("no-effect move unexpectedly applied")
            }
            FileMoveAfterParkResolution::Indeterminate(obligation) => {
                panic!(
                    "restored no-effect move remained indeterminate: {}",
                    obligation.error()
                )
            }
        };
        parked
            .validate_current()
            .expect("no-effect settlement released its park link");
        drop(staged);
        std::fs::remove_file(temporary.path().join("staged.bin")).expect("remove staged source");
        let restored = match parked.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("park restore had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "park restore remained indeterminate: {}",
                    obligation.error()
                )
            }
        };

        drop((restored, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn unsettled_file_move_blocks_hardlink_alias_park_ownership() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("source.bin"), b"payload").expect("source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let source = root
            .open_file(&LeafName::new("source.bin").expect("source leaf"))
            .expect("source capability");
        let authority = root.authority().expect("root authority");
        let operation = authority.enter().expect("move reservation operation");
        let mut movement = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: source.name.clone(),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("destination.bin").expect("destination leaf"),
            },
            None,
            Some(source.identity),
            None,
        )
        .expect("file move reservation");
        let rebind = std::fs::hard_link(
            temporary.path().join("source.bin"),
            temporary.path().join("alias.bin"),
        )
        .and_then(|()| std::fs::remove_file(temporary.path().join("source.bin")));
        if let Err(error) = rebind {
            movement
                .settle(&operation)
                .expect("failed-rebind move settlement");
            panic!("hardlink single-link rebind failed: {error}");
        }
        let alias = match root.open_file(&LeafName::new("alias.bin").expect("alias leaf")) {
            Ok(alias) => alias,
            Err(error) => {
                movement
                    .settle(&operation)
                    .expect("failed-admission move settlement");
                panic!("single-link alias admission failed: {error}");
            }
        };
        let revision = match alias.revision() {
            Ok(revision) => revision,
            Err(error) => {
                drop(alias);
                movement
                    .settle(&operation)
                    .expect("failed-revision move settlement");
                panic!("single-link alias revision failed: {error}");
            }
        };
        let digest: [u8; 32] = Sha256::digest(b"payload").into();
        let request = alias.park_request(ExpectedFileContent::new(revision, digest));

        let outcome = root.park_file_as(
            request,
            LeafName::new("alias.park").expect("alias park leaf"),
        );
        let error = match outcome {
            FileParkOutcome::NoEffect { error, request } => {
                drop(request.file);
                error
            }
            unexpected => {
                movement
                    .settle(&operation)
                    .expect("unexpected-outcome move settlement");
                drop(operation);
                panic!("move-owned inode acquired a second park outcome: {unexpected:?}");
            }
        };
        if let Err(rebind) = std::fs::rename(
            temporary.path().join("alias.bin"),
            temporary.path().join("source.bin"),
        ) {
            movement
                .settle(&operation)
                .expect("failed-restore move settlement");
            panic!("single-link source restoration failed: {rebind}");
        }
        movement
            .settle(&operation)
            .expect("move reservation settlement");
        drop(operation);
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        drop((source, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replacement_handoff_publishes_while_unrelated_park_stays_reserved() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
        std::fs::write(temporary.path().join("unrelated.bin"), b"unrelated payload")
            .expect("unrelated file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let unrelated = park_test_file(
            &root,
            "unrelated.bin",
            "unrelated.park",
            b"unrelated payload",
        );
        let target = root
            .open_file(&LeafName::new("target.bin").expect("target leaf"))
            .expect("target capability");
        let revision = target.revision().expect("target revision");
        let digest: [u8; 32] = Sha256::digest(b"old payload").into();
        let request = target.park_request(ExpectedFileContent::new(revision, digest));
        let staged = test_sealed_stage(&root, b"new payload");

        let (current, displaced) =
            match staged.replace_nondurable(ReplaceDestination::Existing(request)) {
                FileReplaceOutcome::Replaced {
                    current,
                    displaced: Some(displaced),
                } => (current, displaced),
                FileReplaceOutcome::Replaced {
                    displaced: None, ..
                } => {
                    panic!("replacement lost its displaced file")
                }
                FileReplaceOutcome::NoEffect { error, .. } => {
                    panic!("replacement had no effect: {error}")
                }
                FileReplaceOutcome::AppliedUnverified(obligation) => {
                    panic!("replacement remained indeterminate: {}", obligation.error())
                }
            };
        assert_eq!(
            std::fs::read(temporary.path().join("target.bin")).expect("new target payload"),
            b"new payload"
        );
        displaced
            .validate_current()
            .expect("stage disarm released only the replacement handoff");
        assert!(matches!(displaced.remove(), FileRemovalOutcome::Removed));
        let restored = match unrelated.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("unrelated park restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => panic!(
                "unrelated park restoration remained indeterminate: {}",
                obligation.error()
            ),
        };

        drop((current, restored, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_handoff_link_survives_indeterminate_cleanup_until_no_effect() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let mut parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
        let mut staged = test_sealed_stage(&root, b"new payload");
        let source_name = staged.file.name.clone();
        let destination_name = LeafName::new("target.bin").expect("target leaf");
        let attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("publication attempt identity");
        let attempt = platform::prepare_publication(
            attempt_id,
            &staged.file.handle,
            staged.revision.size,
            staged.revision.stamp,
            &root.inner.handle,
            source_name.as_os_str(),
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("prepare replacement publication");
        staged
            .token
            .prepare_promotion(
                &root,
                &destination_name,
                attempt_id,
                attempt,
                Some(&parked.token),
                None,
            )
            .expect("link replacement handoff");

        assert_eq!(
            parked
                .validate_current()
                .expect_err("linked park checkout")
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        let authority = parked.authority().expect("park authority");
        let operation = authority.enter().expect("park rollback operation");
        assert_eq!(
            authority
                .rollback_file_park_registration(&operation, &mut parked.token)
                .expect_err("linked park rollback")
                .kind(),
            io::ErrorKind::WouldBlock,
        );
        drop(operation);

        staged
            .token
            .update(StageRegistryPhase::Unresolved)
            .expect("mark indeterminate publication");
        assert_eq!(
            parked
                .validate_current()
                .expect_err("indeterminate link checkout")
                .kind(),
            io::ErrorKind::WouldBlock,
        );

        let staged_path = temporary.path().join(source_name.as_os_str());
        let displaced_stage_path = temporary.path().join("displaced-stage.bin");
        std::fs::rename(&staged_path, &displaced_stage_path).expect("displace stage binding");
        staged
            .token
            .discard()
            .expect_err("indeterminate cleanup must fail closed");
        assert_eq!(
            parked
                .validate_current()
                .expect_err("failed cleanup link checkout")
                .kind(),
            io::ErrorKind::WouldBlock,
        );

        std::fs::rename(&displaced_stage_path, &staged_path).expect("restore stage binding");
        staged
            .token
            .update(StageRegistryPhase::Sealed)
            .expect("classify publication as no effect");
        parked
            .validate_current()
            .expect("no-effect classification released park link");
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        let restored = match parked.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("park restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!("park restoration was indeterminate: {}", obligation.error())
            }
        };

        drop((restored, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn dropped_replacement_obligation_settles_stage_before_linked_park() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("target.bin"), b"old payload").expect("target file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_file(&root, "target.bin", "target.park", b"old payload");
        let staged = test_sealed_stage(&root, b"new payload");
        let staged_name = staged.file.name.clone();
        let destination_name = LeafName::new("target.bin").expect("target leaf");
        let attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("publication attempt identity");
        let attempt = platform::prepare_publication(
            attempt_id,
            &staged.file.handle,
            staged.revision.size,
            staged.revision.stamp,
            &root.inner.handle,
            staged_name.as_os_str(),
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("prepare replacement publication");
        staged
            .token
            .prepare_promotion(
                &root,
                &destination_name,
                attempt_id,
                attempt.clone(),
                Some(&parked.token),
                None,
            )
            .expect("link replacement handoff");
        let obligation = FileReplaceObligation {
            error: io::Error::other("test replacement remains unresolved"),
            state: Some(Box::new(FileReplaceObligationState::Promoting {
                promotion: Box::new(FilePromotionObligation {
                    error: io::Error::other("test publication remains unresolved"),
                    retained: staged,
                    destination: root.clone(),
                    destination_name: destination_name.clone(),
                    attempt_id,
                    receipt: attempt,
                }),
                displaced: Some(parked),
                fallback: ReplaceDestination::Vacant {
                    parent: root.clone(),
                    name: destination_name,
                },
                receipt: None,
            })),
        };

        drop(obligation);
        drop(root);
        let recovery = match session.revoke() {
            RootRevokeOutcome::Recovery { recovery } => recovery,
            outcome => panic!("dropped replacement did not expose park recovery: {outcome:?}"),
        };
        let file_count = recovery.file_count();
        let directory_count = recovery.directory_count();
        let settlement = recovery.restore_all();
        assert_eq!(file_count, 1, "only the linked park remains recoverable");
        assert_eq!(directory_count, 0);
        assert!(matches!(settlement, RootRevokeOutcome::Revoked));
        assert_eq!(
            std::fs::read(temporary.path().join("target.bin")).expect("restored target"),
            b"old payload",
        );
        assert!(!temporary.path().join(staged_name.as_os_str()).exists());
    }

    #[test]
    fn absolute_descendant_admission_cannot_bypass_directory_park_reservation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("tree")).expect("test tree");
        std::fs::create_dir(temporary.path().join("tree/child")).expect("test child");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "tree", "tree.park");
        let admitted = session
            .admit_absolute_directory(&temporary.path().join("tree.park/child"))
            .expect("absolute parked descendant");
        let authority = admitted.authority().expect("descendant authority");
        let operation = authority.enter().expect("descendant operation");
        let stage_name = LeafName::new("nested-stage.bin").expect("nested stage leaf");
        let error = authority
            .reserve_stage_create(&operation, &admitted, &stage_name, None)
            .expect_err("absolute admission bypassed directory park reservation");
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        drop((operation, authority, admitted));

        let restored = match parked.restore() {
            DirectoryRestoreOutcome::Restored(directory) => directory,
            DirectoryRestoreOutcome::NoEffect { error, .. } => {
                panic!("directory restoration had no effect: {error}")
            }
            DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "directory restoration was indeterminate: {}",
                    obligation.error()
                )
            }
        };
        drop((restored, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn directory_subtree_reservations_reject_nested_owners_in_both_orders() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("tree")).expect("test tree");
        std::fs::create_dir(temporary.path().join("tree/child")).expect("test child");
        std::fs::write(temporary.path().join("tree/child/owned.bin"), b"payload")
            .expect("nested file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let tree = root
            .open_directory(&LeafName::new("tree").expect("tree leaf"))
            .expect("tree capability");
        let child = tree
            .open_directory(&LeafName::new("child").expect("child leaf"))
            .expect("child capability");
        let nested_park = park_test_file(&child, "owned.bin", "owned.park", b"payload");

        let tree = match tree.park_as(LeafName::new("tree.park").expect("tree park leaf")) {
            DirectoryParkOutcome::NoEffect { error, directory } => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                directory
            }
            DirectoryParkOutcome::Parked(_) => {
                panic!("directory park ignored a nested file park owner")
            }
            DirectoryParkOutcome::AppliedUnverified(obligation) => panic!(
                "directory park conflict became indeterminate: {}",
                obligation.error()
            ),
        };
        let nested_file = match nested_park.restore() {
            FileRestoreOutcome::Restored(file) => file,
            FileRestoreOutcome::NoEffect { error, .. } => {
                panic!("nested file restoration had no effect: {error}")
            }
            FileRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "nested file restoration was indeterminate: {}",
                    obligation.error()
                )
            }
        };
        drop((nested_file, child));

        let parked_tree = match tree.park_as(LeafName::new("tree.park").expect("tree park leaf")) {
            DirectoryParkOutcome::Parked(parked) => parked,
            DirectoryParkOutcome::NoEffect { error, .. } => {
                panic!("tree park had no effect: {error}")
            }
            DirectoryParkOutcome::AppliedUnverified(obligation) => {
                panic!("tree park was indeterminate: {}", obligation.error())
            }
        };
        let parked_tree_view = root
            .open_directory(&LeafName::new("tree.park").expect("parked tree leaf"))
            .expect("parked tree capability");
        let parked_child = parked_tree_view
            .open_directory(&LeafName::new("child").expect("parked child leaf"))
            .expect("parked child capability");
        let authority = root.authority().expect("root authority");
        let operation = authority.enter().expect("nested reservation operation");
        assert_eq!(
            MoveEffectToken::reserve(
                &authority,
                &operation,
                NamespaceLeaf {
                    parent: parked_child.clone(),
                    name: LeafName::new("move-source").expect("move source"),
                },
                NamespaceLeaf {
                    parent: parked_child.clone(),
                    name: LeafName::new("move-destination").expect("move destination"),
                },
                None,
                None,
                None,
            )
            .expect_err("directory park must block a nested move")
            .kind(),
            io::ErrorKind::WouldBlock,
        );
        assert_eq!(
            authority
                .reserve_stage_create(
                    &operation,
                    &parked_child,
                    &LeafName::new("nested-stage").expect("nested stage"),
                    None,
                )
                .expect_err("directory park must block a nested stage create")
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        assert_eq!(
            authority
                .reserve_directory_create(
                    &operation,
                    &parked_child,
                    &LeafName::new("nested-directory").expect("nested directory"),
                )
                .expect_err("directory park must block a nested directory create")
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        drop(operation);
        let staged = test_sealed_stage(&root, b"nested publication");
        let staged = match staged.promote_no_replace(
            &root,
            &parked_child,
            &LeafName::new("nested-publication").expect("nested publication"),
        ) {
            FilePromotionOutcome::NoEffect { error, staged } => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                staged
            }
            FilePromotionOutcome::Applied(_) => {
                panic!("directory park allowed a nested stage promotion")
            }
            FilePromotionOutcome::AppliedUnverified(obligation) => panic!(
                "nested stage promotion conflict became indeterminate: {}",
                obligation.error()
            ),
        };
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        let nested_file = parked_child
            .open_file(&LeafName::new("owned.bin").expect("nested file leaf"))
            .expect("nested file capability");
        let revision = nested_file.revision().expect("nested revision");
        let digest: [u8; 32] = Sha256::digest(b"payload").into();
        let request = nested_file.park_request(ExpectedFileContent::new(revision, digest));
        let nested_file = match parked_child.park_file_as(
            request,
            LeafName::new("owned.second-park").expect("second nested park"),
        ) {
            FileParkOutcome::NoEffect { error, request } => {
                assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
                request.file
            }
            FileParkOutcome::Parked(_) => {
                panic!("directory park ignored a nested file park registration")
            }
            FileParkOutcome::Preserved { error, .. } => {
                panic!("directory park conflict changed nested content: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => panic!(
                "nested file park conflict became indeterminate: {}",
                obligation.error()
            ),
        };
        drop((nested_file, parked_child, parked_tree_view, authority));
        let restored_tree = match parked_tree.restore() {
            DirectoryRestoreOutcome::Restored(directory) => directory,
            DirectoryRestoreOutcome::NoEffect { error, .. } => {
                panic!("tree restoration had no effect: {error}")
            }
            DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                panic!("tree restoration was indeterminate: {}", obligation.error())
            }
        };

        drop((restored_tree, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn directory_open_refuses_live_create_before_identity_attachment() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let authority = root.authority().expect("root authority");
        let operation = authority.enter().expect("directory create operation");
        let name = LeafName::new("created").expect("created leaf");
        let mut token = authority
            .reserve_directory_create(&operation, &root, &name)
            .expect("directory create reservation");
        let created = match platform::create_directory(&root.inner.handle, name.as_os_str()) {
            Ok(created) => created,
            Err(_) => panic!("native directory creation failed"),
        };

        let error = root
            .open_directory(&LeafName::new("CREATED").expect("portable alias"))
            .expect_err("live create reservation must block directory admission");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);

        authority.attach_directory_create(&token, created);
        let directory = finish_directory_create(&authority, &operation, &mut token)
            .expect("finish directory creation");
        drop((directory, operation, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn directory_open_rechecks_create_reservation_after_native_open() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("created")).expect("initial directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let authority = root.authority().expect("root authority");
        let name = LeafName::new("created").expect("created leaf");
        let prechecked = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        *authority
            .directory_open_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = Some(DirectoryOpenReservationPause {
            parent: root.inner.identity,
            name: name.clone(),
            prechecked: Arc::clone(&prechecked),
            resume: Arc::clone(&resume),
        });
        let opening = {
            let root = root.clone();
            let name = name.clone();
            std::thread::spawn(move || root.open_directory(&name))
        };
        prechecked.wait();

        std::fs::remove_dir(temporary.path().join("created")).expect("remove initial directory");
        let operation = authority.enter().expect("directory create operation");
        let mut token = authority
            .reserve_directory_create(&operation, &root, &name)
            .expect("directory create reservation");
        let created = match platform::create_directory(&root.inner.handle, name.as_os_str()) {
            Ok(created) => created,
            Err(_) => panic!("native directory creation failed"),
        };
        authority.attach_directory_create(&token, created);
        resume.wait();

        let error = opening
            .join()
            .expect("directory open thread")
            .expect_err("post-open reservation recheck must reject admission");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
        *authority
            .directory_open_pause
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = None;
        let directory = finish_directory_create(&authority, &operation, &mut token)
            .expect("finish directory creation");
        drop((directory, operation, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn identity_pending_directory_create_blocks_nested_reservations() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let authority = root.authority().expect("root authority");
        let name = LeafName::new("created").expect("created leaf");
        let operation = authority.enter().expect("directory create operation");
        let mut token = authority
            .reserve_directory_create(&operation, &root, &name)
            .expect("directory create reservation");
        let created = match platform::create_directory(&root.inner.handle, name.as_os_str()) {
            Ok(created) => created,
            Err(_) => panic!("native directory creation failed"),
        };
        let admitted = session
            .admit_absolute_directory(&temporary.path().join("created"))
            .expect("admit identity-pending directory");
        let nested_operation = authority.enter().expect("nested reservation operation");
        let nested_name = LeafName::new("nested-stage").expect("nested stage leaf");
        assert_eq!(
            authority
                .reserve_stage_create(&nested_operation, &admitted, &nested_name, None)
                .expect_err("identity-pending create must block nested stage")
                .kind(),
            io::ErrorKind::AlreadyExists,
        );
        assert_eq!(
            MoveEffectToken::reserve(
                &authority,
                &nested_operation,
                NamespaceLeaf {
                    parent: admitted.clone(),
                    name: LeafName::new("move-source").expect("move source"),
                },
                NamespaceLeaf {
                    parent: admitted.clone(),
                    name: LeafName::new("move-destination").expect("move destination"),
                },
                None,
                None,
                None,
            )
            .expect_err("identity-pending create must block nested move")
            .kind(),
            io::ErrorKind::WouldBlock,
        );
        drop(nested_operation);

        authority.attach_directory_create(&token, created);
        let directory = finish_directory_create(&authority, &operation, &mut token)
            .expect("finish directory creation");
        drop(operation);
        let operation = authority.enter().expect("settled nested reservation");
        let mut stage = authority
            .reserve_stage_create(&operation, &directory, &nested_name, None)
            .expect("settled create permits nested stage");
        let guard = authority
            .take_stage_create(&operation, &stage)
            .expect("nested stage checkout");
        guard
            .disarm(&mut stage, &operation)
            .expect("nested stage reservation settles");

        drop((operation, directory, admitted, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn unix_observed_publication_runs_real_parent_barriers() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
        let source_path = temporary.path().join("observed-stage.bin");
        let destination_path = temporary
            .path()
            .join("destination")
            .join("observed-publication.bin");
        std::fs::write(&source_path, b"observed payload").expect("observed stage");
        let staged_file = File::open(&source_path).expect("retained observed stage");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let destination = root
            .open_directory(&LeafName::new("destination").expect("destination leaf"))
            .expect("destination capability");
        let attempt_id = 1;
        let (sealed_size, sealed_stamp) =
            platform::file_receipt_fields(&staged_file).expect("observed stage revision");
        let mut attempt = platform::prepare_publication(
            attempt_id,
            &staged_file,
            sealed_size,
            sealed_stamp,
            &root.inner.handle,
            OsStr::new("observed-stage.bin"),
            &destination.inner.handle,
            OsStr::new("observed-publication.bin"),
        )
        .expect("prepare observed publication");
        std::fs::rename(&source_path, &destination_path).expect("externally observed publication");
        platform::settle_publication(
            &mut attempt,
            attempt_id,
            &staged_file,
            &root.inner.handle,
            OsStr::new("observed-stage.bin"),
            &destination.inner.handle,
            OsStr::new("observed-publication.bin"),
        )
        .expect("settle observed publication");
        assert!(!attempt.is_attempted());

        drop((staged_file, root, destination));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn unix_publication_destination_barrier_failure_is_permanently_poisoned() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let source_path = temporary.path().join("stage.bin");
        std::fs::write(&source_path, b"destination barrier payload").expect("stage payload");
        let staged_file = File::open(&source_path).expect("retained stage");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let attempt_id = 1;
        let (sealed_size, sealed_stamp) =
            platform::file_receipt_fields(&staged_file).expect("stage revision");
        let mut receipt = platform::prepare_publication(
            attempt_id,
            &staged_file,
            sealed_size,
            sealed_stamp,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &root.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("prepare publication");
        platform::rename_no_replace(
            &mut receipt,
            attempt_id,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &staged_file,
            &root.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("apply publication");

        {
            let _fault = platform::install_publication_directory_sync_test_outcomes([Err(
                io::ErrorKind::Other,
            )]);
            assert!(
                platform::settle_publication(
                    &mut receipt,
                    attempt_id,
                    &staged_file,
                    &root.inner.handle,
                    OsStr::new("stage.bin"),
                    &root.inner.handle,
                    OsStr::new("published.bin"),
                )
                .is_err()
            );
        }
        {
            let _would_succeed =
                platform::install_publication_directory_sync_test_outcomes([Ok(())]);
            assert!(
                platform::settle_publication(
                    &mut receipt,
                    attempt_id,
                    &staged_file,
                    &root.inner.handle,
                    OsStr::new("stage.bin"),
                    &root.inner.handle,
                    OsStr::new("published.bin"),
                )
                .is_err(),
                "a consumed destination barrier failure must never be retried into durability",
            );
        }

        drop((staged_file, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn unix_publication_source_barrier_failure_is_permanently_poisoned() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("destination")).expect("destination directory");
        let source_path = temporary.path().join("stage.bin");
        std::fs::write(&source_path, b"source barrier payload").expect("stage payload");
        let staged_file = File::open(&source_path).expect("retained stage");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let destination = root
            .open_directory(&LeafName::new("destination").expect("destination leaf"))
            .expect("destination capability");
        let attempt_id = 1;
        let (sealed_size, sealed_stamp) =
            platform::file_receipt_fields(&staged_file).expect("stage revision");
        let mut receipt = platform::prepare_publication(
            attempt_id,
            &staged_file,
            sealed_size,
            sealed_stamp,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &destination.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("prepare publication");
        platform::rename_no_replace(
            &mut receipt,
            attempt_id,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &staged_file,
            &destination.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("apply publication");

        {
            let _fault = platform::install_publication_directory_sync_test_outcomes([
                Ok(()),
                Err(io::ErrorKind::Other),
            ]);
            assert!(
                platform::settle_publication(
                    &mut receipt,
                    attempt_id,
                    &staged_file,
                    &root.inner.handle,
                    OsStr::new("stage.bin"),
                    &destination.inner.handle,
                    OsStr::new("published.bin"),
                )
                .is_err()
            );
        }
        {
            let _would_succeed =
                platform::install_publication_directory_sync_test_outcomes([Ok(()), Ok(())]);
            assert!(
                platform::settle_publication(
                    &mut receipt,
                    attempt_id,
                    &staged_file,
                    &root.inner.handle,
                    OsStr::new("stage.bin"),
                    &destination.inner.handle,
                    OsStr::new("published.bin"),
                )
                .is_err(),
                "a consumed source barrier failure must never be retried into durability",
            );
        }

        drop((staged_file, destination, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn publication_receipt_rejects_reuse_for_another_mutation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let source_path = temporary.path().join("stage.bin");
        let other_path = temporary.path().join("other-stage.bin");
        std::fs::write(&source_path, b"payload").expect("stage payload");
        std::fs::write(&other_path, b"other payload").expect("other payload");
        std::fs::create_dir(temporary.path().join("other")).expect("other parent");
        let staged_file = File::open(&source_path).expect("retained stage");
        let other_file = File::open(&other_path).expect("other retained file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let other_parent = root
            .open_directory(&LeafName::new("other").expect("other parent leaf"))
            .expect("other parent capability");
        let attempt_id = 1;
        let (sealed_size, sealed_stamp) =
            platform::file_receipt_fields(&staged_file).expect("stage revision");
        let mut attempt = platform::prepare_publication(
            attempt_id,
            &staged_file,
            sealed_size,
            sealed_stamp,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &root.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("prepare reported publication");
        platform::rename_no_replace(
            &mut attempt,
            attempt_id,
            &root.inner.handle,
            OsStr::new("stage.bin"),
            &staged_file,
            &root.inner.handle,
            OsStr::new("published.bin"),
        )
        .expect("reported publication");
        let mut receipt = attempt;
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &other_file,
                &root.inner.handle,
                OsStr::new("stage.bin"),
                &root.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id + 1,
                &staged_file,
                &root.inner.handle,
                OsStr::new("stage.bin"),
                &root.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                &other_parent.inner.handle,
                OsStr::new("stage.bin"),
                &root.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                &root.inner.handle,
                OsStr::new("stage.bin"),
                &other_parent.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                &root.inner.handle,
                OsStr::new("other-stage.bin"),
                &root.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                &root.inner.handle,
                OsStr::new("stage.bin"),
                &root.inner.handle,
                OsStr::new("other.bin"),
            )
            .is_err()
        );
        assert!(
            platform::settle_publication(
                &mut receipt,
                attempt_id,
                &staged_file,
                &root.inner.handle,
                OsStr::new("stage.bin"),
                &root.inner.handle,
                OsStr::new("published.bin"),
            )
            .is_ok()
        );

        drop((staged_file, other_file, other_parent, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn windows_publication_requires_reported_write_through_receipt() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let staged = test_sealed_stage(&root, b"unreported payload");
        let attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("publication attempt identity");
        let mut attempt = platform::prepare_publication(
            attempt_id,
            &staged.file.handle,
            staged.revision.size,
            staged.revision.stamp,
            &root.inner.handle,
            staged.file.name.as_os_str(),
            &root.inner.handle,
            OsStr::new("unreported.bin"),
        )
        .expect("prepare unreported publication");

        assert!(
            platform::settle_publication(
                &mut attempt,
                attempt_id,
                &staged.file.handle,
                &root.inner.handle,
                staged.file.name.as_os_str(),
                &root.inner.handle,
                OsStr::new("unreported.bin"),
            )
            .is_err()
        );
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));

        let staged = test_sealed_stage(&root, b"reported payload");
        let published = require_test_promotion(staged.promote_no_replace(
            &root,
            &root,
            &LeafName::new("reported.bin").expect("reported leaf"),
        ));

        drop((published, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn windows_committed_publication_receipt_rejects_cross_mutation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("other-parent")).expect("other parent");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let other_parent = root
            .open_directory(&LeafName::new("other-parent").expect("other parent leaf"))
            .expect("other parent capability");
        let staged = test_sealed_stage(&root, b"committed payload");
        let source_name = staged.file.name.clone();
        let destination_name = LeafName::new("committed.bin").expect("destination leaf");
        let attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("publication attempt identity");
        let restore_attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("restore attempt identity");
        let mut attempt = platform::prepare_publication(
            attempt_id,
            &staged.file.handle,
            staged.revision.size,
            staged.revision.stamp,
            &root.inner.handle,
            source_name.as_os_str(),
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("prepare publication");
        staged
            .token
            .prepare_promotion(
                &root,
                &destination_name,
                attempt_id,
                attempt.clone(),
                None,
                None,
            )
            .expect("track publication attempt");
        platform::rename_no_replace(
            &mut attempt,
            attempt_id,
            &root.inner.handle,
            source_name.as_os_str(),
            &staged.file.handle,
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("report publication");
        staged.token.record_publication(attempt_id, attempt.clone());
        let mut committed = attempt;
        staged
            .token
            .validate_publication_attempt(attempt_id, &committed)
            .expect("validate live publication attempt");
        platform::settle_publication(
            &mut committed,
            attempt_id,
            &staged.file.handle,
            &root.inner.handle,
            source_name.as_os_str(),
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("commit publication");
        staged
            .token
            .record_publication(attempt_id, committed.clone());
        platform::settle_publication(
            &mut committed,
            attempt_id,
            &staged.file.handle,
            &root.inner.handle,
            source_name.as_os_str(),
            &root.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("revalidate committed publication");

        let other = test_sealed_stage(&root, b"other payload");
        let wrong_destination = LeafName::new("other-destination.bin").expect("wrong destination");
        let cases = [
            (
                "attempt",
                attempt_id + 1,
                &staged.file.handle,
                &root.inner.handle,
                source_name.as_os_str(),
                &root.inner.handle,
                destination_name.as_os_str(),
            ),
            (
                "staged file",
                attempt_id,
                &other.file.handle,
                &root.inner.handle,
                source_name.as_os_str(),
                &root.inner.handle,
                destination_name.as_os_str(),
            ),
            (
                "source parent",
                attempt_id,
                &staged.file.handle,
                &other_parent.inner.handle,
                source_name.as_os_str(),
                &root.inner.handle,
                destination_name.as_os_str(),
            ),
            (
                "destination parent",
                attempt_id,
                &staged.file.handle,
                &root.inner.handle,
                source_name.as_os_str(),
                &other_parent.inner.handle,
                destination_name.as_os_str(),
            ),
            (
                "source leaf",
                attempt_id,
                &staged.file.handle,
                &root.inner.handle,
                other.file.name.as_os_str(),
                &root.inner.handle,
                destination_name.as_os_str(),
            ),
            (
                "destination leaf",
                attempt_id,
                &staged.file.handle,
                &root.inner.handle,
                source_name.as_os_str(),
                &root.inner.handle,
                wrong_destination.as_os_str(),
            ),
        ];
        for (
            label,
            candidate_attempt,
            candidate_file,
            candidate_source_parent,
            candidate_source_name,
            candidate_destination_parent,
            candidate_destination_name,
        ) in cases
        {
            assert!(
                platform::settle_publication(
                    &mut committed,
                    candidate_attempt,
                    candidate_file,
                    candidate_source_parent,
                    candidate_source_name,
                    candidate_destination_parent,
                    candidate_destination_name,
                )
                .is_err(),
                "committed receipt accepted a different {label}",
            );
        }

        assert!(matches!(other.discard(), StageDiscardOutcome::Discarded));
        let (restore_size, restore_stamp) =
            platform::file_receipt_fields(&staged.file.handle).expect("restore revision");
        let mut restore = platform::prepare_publication(
            restore_attempt_id,
            &staged.file.handle,
            restore_size,
            restore_stamp,
            &root.inner.handle,
            destination_name.as_os_str(),
            &root.inner.handle,
            source_name.as_os_str(),
        )
        .expect("prepare stage-binding restoration");
        platform::rename_no_replace(
            &mut restore,
            restore_attempt_id,
            &root.inner.handle,
            destination_name.as_os_str(),
            &staged.file.handle,
            &root.inner.handle,
            source_name.as_os_str(),
        )
        .expect("restore stage binding through retained handle");
        staged
            .token
            .update(StageRegistryPhase::Sealed)
            .expect("release publication tracking after restoration");
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        drop((other_parent, root));
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
            admitted
                .revalidate()
                .expect("original binding remains valid");
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
        let expected = admitted.filesystem_identity().expect("admitted identity");
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

    fn park_test_file(root: &Directory, name: &str, park_name: &str, payload: &[u8]) -> ParkedFile {
        let file = root
            .open_file(&LeafName::new(name).expect("record leaf"))
            .expect("record capability");
        let revision = file.revision().expect("record revision");
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let request = file.park_request(ExpectedFileContent::new(revision, digest));
        match root.park_file_as(request, LeafName::new(park_name).expect("preserved leaf")) {
            FileParkOutcome::Parked(parked) => parked,
            FileParkOutcome::NoEffect { error, .. } => {
                panic!("preservation park had no effect: {error}")
            }
            FileParkOutcome::Preserved { error, .. } => {
                panic!("preservation park retained changed content: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                panic!("preservation park was not verified: {}", obligation.error())
            }
        }
    }

    fn restored_file_park_obligation(
        root: &Directory,
        name: &str,
        park_name: &str,
        payload: &[u8],
    ) -> FileParkObligation {
        let name = LeafName::new(name).expect("record leaf");
        let park_name = LeafName::new(park_name).expect("park leaf");
        let file = root.open_file(&name).expect("record capability");
        let revision = file.revision().expect("record revision");
        let digest: [u8; 32] = Sha256::digest(payload).into();
        let request = file.park_request(ExpectedFileContent::new(revision, digest));
        let authority = root.authority().expect("root authority");
        let operation = authority.enter().expect("park operation");
        let cleanup =
            platform::open_parked_file(&root.inner.handle, name.as_os_str(), request.file.identity)
                .expect("retained cleanup handle");
        let token = authority
            .reserve_file_park(&operation, &request, park_name.clone(), cleanup)
            .expect("park reservation");
        let mut guard = authority
            .take_file_park(&operation, &token)
            .expect("park record");
        match platform::park_file_no_replace(
            &root.inner.handle,
            name.as_os_str(),
            &request.file.handle,
            request.file.identity,
            park_name.as_os_str(),
            &guard.record().cleanup,
        ) {
            Ok(()) => {}
            Err(platform::ParkFileError::NoEffect(error)) => {
                panic!("test file park had no effect: {error}")
            }
            Err(platform::ParkFileError::AppliedUnverified(error)) => {
                panic!("test file park was unverified: {error}")
            }
        }
        {
            let record = guard.record_mut();
            platform::restore_parked_file(
                &record.parent.inner.handle,
                record.name.as_os_str(),
                &mut record.cleanup,
                record.identity,
                record.original_name.as_os_str(),
            )
            .expect("restore parked file");
        }
        drop(guard);
        drop(operation);
        FileParkObligation {
            error: io::Error::other("test restored park requires settlement"),
            request: Some(request),
            token,
            park_name,
            phase: FileParkPhase::RestoringRejectedReceipt,
            digest_verified: false,
            restored_proof_pause: None,
        }
    }

    #[test]
    fn restored_park_refreshes_revision_and_classifies_current() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("current.bin"), b"current payload")
            .expect("current payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let obligation = restored_file_park_obligation(
            &root,
            "current.bin",
            "current.parked",
            b"current payload",
        );
        let request = match obligation.reconcile() {
            FileParkResolution::NoEffect(request) => request,
            FileParkResolution::Parked(_) => panic!("restored file remained parked"),
            FileParkResolution::Preserved { error, .. } => {
                panic!("unchanged restored file was only preserved: {error}")
            }
            FileParkResolution::Indeterminate(obligation) => {
                panic!(
                    "restored file remained indeterminate: {}",
                    obligation.error()
                )
            }
        };
        let request = match request.classify_source(&root) {
            Ok(FileParkRequestSource::Current(request)) => request,
            Ok(FileParkRequestSource::Displaced) => {
                panic!("refreshed restored request was displaced")
            }
            Err(error) => {
                let (error, _) = error.into_parts();
                panic!("refreshed restored request could not be classified: {error}")
            }
        };
        drop(request);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn restored_park_preserves_stable_digest_drift_without_delete_authority() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("changed.bin"), b"original payload")
            .expect("original payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let obligation = restored_file_park_obligation(
            &root,
            "changed.bin",
            "changed.parked",
            b"original payload",
        );
        std::fs::write(temporary.path().join("changed.bin"), b"modified payload")
            .expect("modified payload");
        let file = match obligation.reconcile() {
            FileParkResolution::Preserved { file, .. } => file,
            FileParkResolution::NoEffect(_) => {
                panic!("changed restored file returned deletion authority")
            }
            FileParkResolution::Parked(_) => panic!("changed restored file remained parked"),
            FileParkResolution::Indeterminate(obligation) => {
                panic!(
                    "stable changed file remained indeterminate: {}",
                    obligation.error()
                )
            }
        };
        assert_eq!(
            std::fs::read(temporary.path().join("changed.bin")).expect("preserved payload"),
            b"modified payload",
        );
        drop(file);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn restored_park_retains_racing_content_until_a_stable_preservation_proof() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("racing.bin"), b"original payload")
            .expect("original payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let mut obligation = restored_file_park_obligation(
            &root,
            "racing.bin",
            "racing.parked",
            b"original payload",
        );
        let hashed = Arc::new(std::sync::Barrier::new(2));
        let resume = Arc::new(std::sync::Barrier::new(2));
        obligation.restored_proof_pause = Some(RestoredFileProofPause {
            hashed: Arc::clone(&hashed),
            resume: Arc::clone(&resume),
        });
        let settlement = std::thread::spawn(move || obligation.reconcile());
        hashed.wait();
        std::fs::write(temporary.path().join("racing.bin"), b"modified payload")
            .expect("racing payload");
        resume.wait();
        let obligation = match settlement.join().expect("settlement thread") {
            FileParkResolution::Indeterminate(obligation) => obligation,
            FileParkResolution::NoEffect(_) => {
                panic!("racing restored file returned deletion authority")
            }
            FileParkResolution::Preserved { .. } => {
                panic!("racing restored file was classified from unequal receipts")
            }
            FileParkResolution::Parked(_) => panic!("racing restored file remained parked"),
        };
        let file = match obligation.reconcile() {
            FileParkResolution::Preserved { file, .. } => file,
            FileParkResolution::NoEffect(_) => {
                panic!("changed restored file returned deletion authority")
            }
            FileParkResolution::Indeterminate(obligation) => {
                panic!(
                    "stable retry remained indeterminate: {}",
                    obligation.error()
                )
            }
            FileParkResolution::Parked(_) => panic!("stable retry remained parked"),
        };
        drop(file);
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn restored_park_retains_share_locked_content_until_settlement() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("locked.bin"), b"original payload")
            .expect("original payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let obligation = restored_file_park_obligation(
            &root,
            "locked.bin",
            "locked.parked",
            b"original payload",
        );

        std::fs::write(temporary.path().join("locked.bin"), b"modified payload")
            .expect_err("retained Windows cleanup authority must refuse a conflicting writer");
        match obligation.reconcile() {
            FileParkResolution::NoEffect(request) => drop(request),
            FileParkResolution::Preserved { error, .. } => {
                panic!("share-locked content unexpectedly drifted: {error}")
            }
            FileParkResolution::Indeterminate(obligation) => {
                panic!(
                    "share-locked restored file remained indeterminate: {}",
                    obligation.error()
                )
            }
            FileParkResolution::Parked(_) => panic!("restored file remained parked"),
        }
        assert_eq!(
            std::fs::read(temporary.path().join("locked.bin")).expect("share-locked payload"),
            b"original payload",
        );
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    fn park_test_directory(root: &Directory, name: &str, park_name: &str) -> ParkedDirectory {
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

    fn test_recoverable_stage(
        root: &Directory,
        destination: &LeafName,
        bytes: &[u8],
    ) -> (SealedStagedFile, RecoveryRegistration) {
        let mut staged = match root.create_recoverable_stage(destination) {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => {
                panic!("recovery stage creation failed: {error}")
            }
            FileCreateOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "recovery stage creation was not verified: {}",
                    obligation.error()
                )
            }
        };
        staged.write_all(bytes).expect("recovery stage bytes");
        let sealed = staged.seal().expect("sealed recovery stage");
        let registration = root
            .authority()
            .expect("recovery authority")
            .operations
            .lock()
            .expect("recovery state")
            .stages
            .get(&sealed.token.id)
            .expect("registered recovery stage")
            .recovery
            .expect("recovery registration");
        (sealed, registration)
    }

    fn persist_test_recovery_fixture(
        session: &RootSession,
        root_path: &Path,
        operation_id: [u8; 16],
        destination: &str,
        phase: RecoveryPhase,
        payload: &[u8],
        create_stage: bool,
    ) -> (RecoveryRegistration, RecoveryName) {
        persist_test_recovery_fixture_in(
            session,
            root_path,
            operation_id,
            Vec::new(),
            destination,
            phase,
            payload,
            create_stage,
        )
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the fixture names every durable record field and physical carrier"
    )]
    fn persist_test_recovery_fixture_in(
        session: &RootSession,
        root_path: &Path,
        operation_id: [u8; 16],
        destination_parent: Vec<RecoveryName>,
        destination: &str,
        phase: RecoveryPhase,
        payload: &[u8],
        create_stage: bool,
    ) -> (RecoveryRegistration, RecoveryName) {
        let stage = recovery_stage_leaf(operation_id);
        let stage_parent = destination_parent
            .iter()
            .fold(root_path.to_path_buf(), |path, component| {
                path.join(component.as_str())
            });
        let mut record = RecoveryRecord {
            operation_id,
            phase: RecoveryPhase::StagePrepared,
            destination_parent,
            destination_leaf: RecoveryName::new_exact(destination).expect("recovery destination"),
            old: None,
            new: None,
        };
        let mut journal =
            RecoveryJournal::load(&session.authority.lease).expect("load recovery fixture journal");
        let registration = journal.reserve(&record).expect("reserve recovery fixture");
        journal
            .create_reserved(&session.authority.lease, registration, record.clone())
            .expect("persist prepared recovery fixture");
        if create_stage {
            let path = stage_parent.join(stage.as_str());
            std::fs::write(&path, payload).expect("write recovery fixture stage");
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .expect("open recovery fixture stage")
                .sync_all()
                .expect("sync recovery fixture stage");
        }
        if phase != RecoveryPhase::StagePrepared {
            record.phase = RecoveryPhase::StageSealed;
            record.new = Some(recovery::RecoveryFileProof {
                size: payload.len() as u64,
                sha256: Sha256::digest(payload).into(),
            });
            journal
                .advance(&session.authority.lease, registration, record.clone())
                .expect("persist sealed recovery fixture");
        }
        if phase != record.phase {
            if phase == RecoveryPhase::RemoveCommitted {
                record.phase = RecoveryPhase::PublishPrepared;
                journal
                    .advance(&session.authority.lease, registration, record.clone())
                    .expect("persist prepared publication fixture");
            }
            record.phase = phase;
            journal
                .advance(&session.authority.lease, registration, record)
                .expect("persist final recovery fixture phase");
        }
        (registration, stage)
    }

    #[expect(
        clippy::too_many_arguments,
        reason = "the fixture names every durable record field and physical carrier"
    )]
    fn persist_test_replacement_fixture(
        session: &RootSession,
        root_path: &Path,
        operation_id: [u8; 16],
        destination: &str,
        phase: RecoveryPhase,
        old_payload: &[u8],
        new_payload: &[u8],
        carriers: [Option<&[u8]>; 3],
    ) -> (RecoveryRegistration, RecoveryName, RecoveryName) {
        let stage = recovery_stage_leaf(operation_id);
        let park = recovery_park_leaf(operation_id);
        let old = recovery::RecoveryFileProof {
            size: old_payload.len() as u64,
            sha256: Sha256::digest(old_payload).into(),
        };
        let new = recovery::RecoveryFileProof {
            size: new_payload.len() as u64,
            sha256: Sha256::digest(new_payload).into(),
        };
        let mut record = RecoveryRecord {
            operation_id,
            phase: RecoveryPhase::StagePrepared,
            destination_parent: Vec::new(),
            destination_leaf: RecoveryName::new_exact(destination).expect("recovery destination"),
            old: Some(old),
            new: None,
        };
        let mut journal =
            RecoveryJournal::load(&session.authority.lease).expect("load replacement journal");
        let registration = journal
            .reserve(&record)
            .expect("reserve replacement fixture");
        journal
            .create_reserved(&session.authority.lease, registration, record.clone())
            .expect("persist prepared replacement fixture");
        let phases: &[RecoveryPhase] = match phase {
            RecoveryPhase::StagePrepared => &[],
            RecoveryPhase::StageSealed => &[RecoveryPhase::StageSealed],
            RecoveryPhase::ReplacePrepared => {
                &[RecoveryPhase::StageSealed, RecoveryPhase::ReplacePrepared]
            }
            RecoveryPhase::PublishPrepared => &[
                RecoveryPhase::StageSealed,
                RecoveryPhase::ReplacePrepared,
                RecoveryPhase::PublishPrepared,
            ],
            RecoveryPhase::RemovePrepared => {
                &[RecoveryPhase::StageSealed, RecoveryPhase::RemovePrepared]
            }
            RecoveryPhase::RemoveCommitted => &[
                RecoveryPhase::StageSealed,
                RecoveryPhase::ReplacePrepared,
                RecoveryPhase::PublishPrepared,
                RecoveryPhase::RemoveCommitted,
            ],
        };
        for &phase in phases {
            record.phase = phase;
            if phase == RecoveryPhase::StageSealed {
                record.new = Some(new);
            }
            journal
                .advance(&session.authority.lease, registration, record.clone())
                .expect("persist replacement fixture phase");
        }
        for (name, payload) in [
            (stage.as_str(), carriers[0]),
            (destination, carriers[1]),
            (park.as_str(), carriers[2]),
        ] {
            let Some(payload) = payload else { continue };
            let path = root_path.join(name);
            std::fs::write(&path, payload).expect("write replacement fixture carrier");
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(path)
                .expect("open replacement fixture carrier")
                .sync_all()
                .expect("sync replacement fixture carrier");
        }
        (registration, stage, park)
    }

    fn persist_test_state_successor(
        session: &RootSession,
        registration: RecoveryRegistration,
        payload: &[u8],
    ) {
        persist_test_state_successor_batch(session, &[(registration, payload)], &[registration]);
    }

    fn persist_test_state_successor_batch(
        session: &RootSession,
        entries: &[(RecoveryRegistration, &[u8])],
        cleared: &[RecoveryRegistration],
    ) {
        persist_test_state_successor_batch_with_owner(
            session,
            b"operation-journals",
            entries,
            cleared,
        );
    }

    fn persist_test_state_successor_batch_with_owner(
        session: &RootSession,
        owner_id: &[u8],
        entries: &[(RecoveryRegistration, &[u8])],
        cleared: &[RecoveryRegistration],
    ) {
        let mut entries = entries.to_vec();
        entries.sort_by_key(|(registration, _)| registration.operation_id);
        let mut manifest = Vec::with_capacity(3 + entries.len() * 56);
        manifest.extend_from_slice(&1_u16.to_le_bytes());
        manifest.push(u8::try_from(entries.len()).expect("State fixture count"));
        for (registration, payload) in &entries {
            manifest.extend_from_slice(&registration.operation_id);
            manifest.extend_from_slice(&(payload.len() as u64).to_le_bytes());
            manifest.extend_from_slice(&Sha256::digest(payload));
        }
        let mut journal = RecoveryJournal::load(&session.authority.lease)
            .expect("load State successor fixture journal");
        let registrations = entries
            .iter()
            .map(|(registration, _)| *registration)
            .collect::<Vec<_>>();
        let owner = journal
            .create_successor(
                &session.authority.lease,
                successor::SuccessorRecord {
                    owner_class: successor::SuccessorOwnerClass::State,
                    owner_schema: 1,
                    owner_id: owner_id.to_vec(),
                    transfer_id: [0; 16],
                    old_payload: None,
                    new_payload: Some(manifest),
                    acknowledgements: Vec::new(),
                },
                &registrations,
            )
            .unwrap_or_else(|(error, owner)| {
                if let Some(owner) = owner {
                    recovery::disarm_successor_owner_for_restart(owner);
                }
                panic!("persist State successor fixture: {error}")
            });
        for registration in cleared {
            journal
                .clear(&session.authority.lease, *registration)
                .expect("tombstone acknowledged recovery fixture");
        }
        recovery::disarm_successor_owner_for_restart(owner);
    }

    fn test_file_identity(path: &Path) -> platform::Identity {
        let file = File::open(path).expect("open test identity carrier");
        platform::file_identity(&file).expect("test carrier identity")
    }

    #[cfg(unix)]
    fn assert_test_recovery_terminal(
        session: &RootSession,
        registration: RecoveryRegistration,
        _windows_phase: RecoveryPhase,
        _windows_files: &[platform::Identity],
    ) {
        let state = session.authority.operations.lock().expect("recovery state");
        assert!(state.recovery.record(registration).is_none());
        assert!(
            state
                .recovery_orphans
                .iter()
                .all(|orphan| orphan.registration != registration)
        );
    }

    #[cfg(windows)]
    fn assert_test_recovery_terminal(
        session: &RootSession,
        registration: RecoveryRegistration,
        windows_phase: RecoveryPhase,
        windows_files: &[platform::Identity],
    ) {
        let state = session.authority.operations.lock().expect("recovery state");
        assert_eq!(
            state
                .recovery
                .record(registration)
                .expect("retained Windows recovery record")
                .phase,
            windows_phase
        );
        assert!(
            state
                .recovery_orphans
                .iter()
                .find(|orphan| orphan.registration == registration)
                .expect("retained Windows recovery orphan")
                .files
                .as_slice()
                == windows_files
        );
    }

    fn test_publication_attempt(
        staged: &SealedStagedFile,
        destination: &Directory,
        destination_name: &LeafName,
    ) -> (u64, platform::PublicationReceipt) {
        let attempt_id = staged
            .token
            .allocate_publication_attempt()
            .expect("publication attempt identity");
        let receipt = platform::prepare_publication(
            attempt_id,
            &staged.file.handle,
            staged.revision.size,
            staged.revision.stamp,
            &staged.file.parent.inner.handle,
            staged.file.name.as_os_str(),
            &destination.inner.handle,
            destination_name.as_os_str(),
        )
        .expect("publication attempt");
        (attempt_id, receipt)
    }

    fn require_test_promotion(outcome: FilePromotionOutcome) -> FileCapability {
        match outcome {
            FilePromotionOutcome::Applied(file) => file,
            FilePromotionOutcome::NoEffect { error, staged } => {
                assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
                panic!("stage promotion had no effect: {error}");
            }
            FilePromotionOutcome::AppliedUnverified(obligation) => {
                let initial_error = obligation.error().to_string();
                match (*obligation).reconcile() {
                    FilePromotionResolution::Applied(file) => file,
                    FilePromotionResolution::NoEffect(staged) => {
                        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
                        panic!("stage promotion reconciled to no effect: {initial_error}");
                    }
                    FilePromotionResolution::Indeterminate(obligation) => {
                        panic!(
                            "stage promotion remained indeterminate after reconciliation: {}; initial error: {initial_error}",
                            obligation.error()
                        )
                    }
                }
            }
        }
    }

    fn require_test_promotion_no_effect(outcome: FilePromotionOutcome) -> SealedStagedFile {
        match outcome {
            FilePromotionOutcome::NoEffect { staged, .. } => staged,
            FilePromotionOutcome::Applied(_) => panic!("stage promotion unexpectedly applied"),
            FilePromotionOutcome::AppliedUnverified(obligation) => {
                match (*obligation).reconcile() {
                    FilePromotionResolution::NoEffect(staged) => staged,
                    FilePromotionResolution::Applied(_) => {
                        panic!("stage promotion reconciliation unexpectedly applied")
                    }
                    FilePromotionResolution::Indeterminate(obligation) => panic!(
                        "stage promotion remained indeterminate: {}",
                        obligation.error()
                    ),
                }
            }
        }
    }

    fn require_test_stage_discard(outcome: StageDiscardOutcome) {
        match outcome {
            StageDiscardOutcome::Discarded => {}
            StageDiscardOutcome::AppliedUnverified(obligation) => {
                let initial_error = obligation.error().to_string();
                match obligation.reconcile() {
                    StageDiscardResolution::Discarded => {}
                    StageDiscardResolution::Indeterminate(obligation) => panic!(
                        "stage discard remained indeterminate: {}; initial error: {initial_error}",
                        obligation.error()
                    ),
                }
            }
        }
    }

    fn require_test_state_batch(mut outcome: StateFileBatchOutcome) -> Vec<FileCapability> {
        for _ in 0..8 {
            outcome = match outcome {
                StateFileBatchOutcome::Replaced(files) => return files,
                StateFileBatchOutcome::NoEffect { error, .. } => {
                    panic!("State batch had no effect: {error}")
                }
                StateFileBatchOutcome::AppliedUnverified(obligation) => obligation.reconcile(),
            };
        }
        panic!("State batch remained unsettled")
    }

    fn require_test_state_file(outcome: StateFileBatchOutcome) -> FileCapability {
        let mut files = require_test_state_batch(outcome);
        assert_eq!(files.len(), 1);
        files.pop().expect("single State batch file")
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
                Some(file.identity),
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
                None,
                None,
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
        std::fs::write(temporary.path().join("second/source.bin"), b"source").expect("source file");
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
        std::fs::write(temporary.path().join("domain/before.bin"), b"before").expect("before file");
        std::fs::write(temporary.path().join("domain/after.bin"), b"after").expect("after file");
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
        for index in 0..MAX_EFFECTS_PER_OWNER {
            std::fs::write(
                temporary
                    .path()
                    .join("domain")
                    .join(format!("source-{index}.bin")),
                b"source",
            )
            .expect("unique source file");
        }
        std::fs::write(
            temporary.path().join("domain/overflow-source.bin"),
            b"overflow source",
        )
        .expect("overflow source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let first_file = domain
            .open_file(&LeafName::new("source-0.bin").expect("source leaf"))
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
            assert!(
                state
                    .active_effect_owners
                    .get(&owner.state.id)
                    .is_some_and(|active| Arc::ptr_eq(active, &owner.state))
            );
        }

        let mut queued = Vec::with_capacity(MAX_EFFECTS_PER_OWNER - 1);
        for index in 1..MAX_EFFECTS_PER_OWNER {
            let file = domain
                .open_file(&LeafName::new(format!("source-{index}.bin")).expect("source leaf"))
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
            .open_file(&LeafName::new("overflow-source.bin").expect("source leaf"))
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
        std::fs::write(temporary.path().join("domain/source.bin"), b"source").expect("source file");
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
        std::fs::write(temporary.path().join("domain/source.bin"), b"source").expect("source file");
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
        std::fs::write(temporary.path().join("domain/source.bin"), b"source").expect("source file");
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
        std::fs::write(temporary.path().join("record.bin"), b"payload").expect("test file");
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
        assert_eq!(directory_refusal.error().kind(), io::ErrorKind::WouldBlock);
        let directory = match parked_directory.restore() {
            DirectoryRestoreOutcome::Restored(directory) => directory,
            DirectoryRestoreOutcome::NoEffect { error, .. } => {
                panic!("raw directory restore had no effect: {error}")
            }
            DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "raw directory restore was not verified: {}",
                    obligation.error()
                )
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
            temporary
                .path()
                .join("domain/victim/nested/deeper/payload.bin"),
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
        std::fs::write(
            temporary.path().join("victim/nested/retained.bin"),
            b"retained",
        )
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
        directory
            .entries(1)
            .expect("source directory remains admitted");

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
        symlink(
            external.path(),
            temporary.path().join("victim/nested/external"),
        )
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
        std::fs::write(
            temporary.path().join("victim/nested/original.bin"),
            b"original",
        )
        .expect("original payload");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let parked = park_test_directory(&root, "victim", "victim.deleted");
        std::fs::rename(
            temporary.path().join("victim.deleted"),
            temporary.path().join("victim.moved"),
        )
        .expect("move parked tree out of its binding");
        std::fs::create_dir(temporary.path().join("victim.deleted")).expect("replacement root");
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

        let deepest = (0..=platform::MAX_TREE_CLEAR_DEPTH)
            .fold(temporary.path().join("victim.deleted"), |path, _| {
                path.join("d")
            });
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
        std::fs::write(temporary.path().join("restore.bin"), b"restore").expect("restore file");
        std::fs::write(temporary.path().join("preserve.bin"), b"preserve").expect("preserve file");
        std::fs::create_dir(temporary.path().join("folder")).expect("restore directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let owner = root.create_effect_owner().expect("effect owner");
        let restore_file = park_test_file(&root, "restore.bin", "restore.parked", b"restore");
        owner
            .retain_parked_file_restore(restore_file)
            .expect("retain file restore");
        let preserve_file = park_test_file(&root, "preserve.bin", "preserve.parked", b"preserve");
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
            std::fs::read(temporary.path().join("preserve.parked")).expect("preserved file"),
            b"preserve",
        );
        assert!(temporary.path().join("folder").is_dir());
        assert!(temporary.path().join("preserved-directory").is_dir());
    }

    #[test]
    fn effect_owner_settlement_is_fifo_and_stops_at_first_unresolved_move() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("domain")).expect("domain anchor");
        std::fs::write(temporary.path().join("domain/first.bin"), b"first").expect("first file");
        std::fs::write(temporary.path().join("domain/second.bin"), b"second").expect("second file");
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
            let Some(OwnedEffect::FileMove(OwnedFileMove::Pending { obligation, .. })) =
                records.effects.get_mut(&first.id)
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
        let promotion_name = LeafName::new("promotion.bin").expect("promotion leaf");
        let (promotion_attempt_id, promotion_attempt) =
            test_publication_attempt(&promoted_stage, &root, &promotion_name);
        let promotion = FilePromotionObligation {
            error: io::Error::other("test promotion requires settlement"),
            retained: promoted_stage,
            destination: root.clone(),
            destination_name: promotion_name,
            attempt_id: promotion_attempt_id,
            receipt: promotion_attempt,
        };
        let promotion = owner
            .retain_file_promotion(Box::new(promotion))
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
        let replacement_name = LeafName::new("replacement.bin").expect("replacement leaf");
        let (replacement_attempt_id, replacement_attempt) =
            test_publication_attempt(&replacement_stage, &root, &replacement_name);
        let replacement_promotion = FilePromotionObligation {
            error: io::Error::other("test replacement promotion requires settlement"),
            retained: replacement_stage,
            destination: root.clone(),
            destination_name: replacement_name.clone(),
            attempt_id: replacement_attempt_id,
            receipt: replacement_attempt,
        };
        let replacement = owner
            .retain_file_replace(FileReplaceObligation {
                error: io::Error::other("test replacement requires settlement"),
                state: Some(Box::new(FileReplaceObligationState::Promoting {
                    promotion: Box::new(replacement_promotion),
                    displaced: None,
                    fallback: ReplaceDestination::Vacant {
                        parent: root.clone(),
                        name: replacement_name,
                    },
                    receipt: None,
                })),
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
        std::fs::write(temporary.path().join("destination.bin"), b"old").expect("destination file");
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
            FileParkOutcome::Preserved { error, .. } => {
                panic!("destination park retained changed content: {error}")
            }
            FileParkOutcome::AppliedUnverified(obligation) => {
                panic!("destination park was not verified: {}", obligation.error())
            }
        };
        let staged = test_sealed_stage(&root, b"new");
        let receipt = owner
            .retain_file_replace(FileReplaceObligation {
                error: io::Error::other("test replacement rollback requires settlement"),
                state: Some(Box::new(FileReplaceObligationState::RestoreParked {
                    parked,
                    staged,
                    receipt: expected,
                })),
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
        let request = match destination {
            ReplaceDestination::Existing(request) => request,
            ReplaceDestination::Vacant { .. } => {
                panic!("restored replacement destination became vacant")
            }
            ReplaceDestination::Preserved(_) => {
                panic!("unchanged restored replacement lost retry authority")
            }
        };
        let parent = request.file.parent.clone();
        let request = match request.classify_source(&parent) {
            Ok(FileParkRequestSource::Current(request)) => request,
            Ok(FileParkRequestSource::Displaced) => {
                panic!("refreshed restored replacement request was displaced")
            }
            Err(error) => {
                let (error, _) = error.into_parts();
                panic!("refreshed restored replacement request was stale: {error}")
            }
        };
        drop((request, parent));
        assert!(matches!(staged.discard(), StageDiscardOutcome::Discarded));
        assert!(matches!(refusal.retry(), RootRevokeOutcome::Revoked));
        assert_eq!(
            std::fs::read(temporary.path().join("destination.bin")).expect("restored destination"),
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
        for index in 0..MAX_EFFECTS_PER_OWNER {
            std::fs::write(
                temporary
                    .path()
                    .join("domain")
                    .join(format!("source-{index}.bin")),
                b"source",
            )
            .expect("unique source file");
        }
        std::fs::write(
            temporary.path().join("domain/overflow-source.bin"),
            b"overflow source",
        )
        .expect("overflow source file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let domain = root
            .open_directory(&LeafName::new("domain").expect("domain leaf"))
            .expect("domain capability");
        let owner = domain.create_effect_owner().expect("effect owner");
        let mut receipts = Vec::with_capacity(MAX_EFFECTS_PER_OWNER);
        for index in 0..MAX_EFFECTS_PER_OWNER {
            let source_name =
                LeafName::new(format!("source-{index}.bin")).expect("unique source leaf");
            let file = domain.open_file(&source_name).expect("source capability");
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
            .open_file(&LeafName::new("overflow-source.bin").expect("overflow source leaf"))
            .expect("overflow source capability");
        let failure = owner
            .retain_file_move(test_file_move_obligation(
                overflow_file,
                domain.clone(),
                "overflow-target.bin",
                false,
            ))
            .expect_err("effect capacity must apply backpressure");
        let (error, mut overflow) = failure.into_parts();
        let error_kind = error.kind();
        let overflow_file = overflow.file.take().expect("returned overflow file");
        let authority = overflow_file
            .parent
            .authority()
            .expect("overflow authority");
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
        assert_eq!(error_kind, io::ErrorKind::WouldBlock);

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
                None,
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
            .begin_terminal_drain(false)
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
        std::fs::write(temporary.path().join("source/moved.bin"), b"moved").expect("moved source");
        std::fs::write(temporary.path().join("source/collision.bin"), b"source")
            .expect("collision source");
        std::fs::write(
            temporary.path().join("destination/collision.bin"),
            b"destination",
        )
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
        assert_eq!(moved.read_bounded(16).expect("moved bytes"), b"moved");

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
        assert!(temporary.path().join(ROOT_LEASE_NAME).is_file());
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        receipt.release().expect("release clear receipt");
        drop(acquire_test_root(temporary.path()));
    }

    #[cfg(unix)]
    #[test]
    fn root_lease_is_a_retained_single_link_file_and_serializes_sessions() {
        use std::os::unix::fs::MetadataExt;

        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        let metadata = std::fs::symlink_metadata(&lease).expect("lease metadata");
        assert!(metadata.file_type().is_file());
        assert_eq!(metadata.nlink(), 1);
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));

        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        assert!(lease.is_file(), "lease file should remain reusable");
        assert!(matches!(
            acquire_test_root(temporary.path()).revoke(),
            RootRevokeOutcome::Revoked
        ));
    }

    #[cfg(unix)]
    #[test]
    fn root_lease_rejects_a_noncanonical_portable_alias_before_acquisition() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let alias = temporary.path().join(".AXIAL-ROOT.LEASE");
        std::fs::write(&alias, b"user-owned alias").expect("portable lease alias");

        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(_)
        ));
        assert!(alias.is_file());
        assert!(!temporary.path().join(ROOT_LEASE_NAME).exists());
    }

    #[test]
    fn lease_name_class_scan_cache_invalidates_on_root_revision_change() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        platform::reset_lease_name_class_scan_count();

        for _ in 0..8 {
            drop(session.authority.enter().expect("cached lease validation"));
        }
        assert_eq!(platform::lease_name_class_scan_count(), 0);

        let sibling = temporary.path().join("cooperative-sibling");
        std::fs::write(&sibling, b"owned").expect("create cooperative sibling");
        drop(session.authority.enter().expect("changed-root validation"));
        assert_eq!(platform::lease_name_class_scan_count(), 1);
        drop(
            session
                .authority
                .enter()
                .expect("recached lease validation"),
        );
        assert_eq!(platform::lease_name_class_scan_count(), 1);

        std::fs::remove_file(sibling).expect("remove cooperative sibling");
        drop(
            session
                .authority
                .enter()
                .expect("second changed-root validation"),
        );
        assert_eq!(platform::lease_name_class_scan_count(), 2);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn windows_root_lease_accepts_exact_and_rejects_noncanonical_spelling() {
        let exact = tempfile::tempdir().expect("exact temporary root");
        File::create(exact.path().join(ROOT_LEASE_NAME))
            .expect("create exact lease")
            .set_len(recovery::RECOVERY_CONTROL_BYTES)
            .expect("size exact lease");
        assert!(matches!(
            acquire_test_root(exact.path()).revoke(),
            RootRevokeOutcome::Revoked
        ));

        let noncanonical = tempfile::tempdir().expect("noncanonical temporary root");
        let alias = noncanonical.path().join(".AXIAL-ROOT.LEASE");
        std::fs::write(&alias, b"user-owned alias").expect("noncanonical lease alias");
        assert!(matches!(
            RootSession::acquire(noncanonical.path()),
            RootSessionAcquireOutcome::NoEffect(_)
        ));
        let names = std::fs::read_dir(noncanonical.path())
            .expect("enumerate noncanonical root")
            .map(|entry| entry.expect("root entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(names, [OsString::from(".AXIAL-ROOT.LEASE")]);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn retained_root_lease_rejects_a_new_portable_alias_before_control_io() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let alias = temporary.path().join(".AXIAL-ROOT.LEASE");
        std::fs::write(&alias, b"user-owned alias").expect("portable lease alias");

        let mut byte = [0; 1];
        assert!(
            platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte)
                .is_err()
        );
        std::fs::remove_file(alias).expect("remove portable lease alias");
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_control_refuses_a_displaced_root_and_replacement_lease() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root = temporary.path().join("app");
        let displaced = temporary.path().join("displaced-app");
        std::fs::create_dir(&root).expect("application root");
        let first = acquire_test_root(&root);
        platform::recovery_control_initialize_len(&first.authority.lease)
            .expect("initialize recovery control");
        platform::recovery_control_write_all_at(&first.authority.lease, 0, &[0x11])
            .expect("write recovery control");
        platform::recovery_control_sync(&first.authority.lease).expect("sync recovery control");

        std::fs::rename(&root, &displaced).expect("displace application root");
        std::fs::create_dir(&root).expect("replacement application root");
        let replacement = acquire_test_root(&root);
        assert!(
            platform::recovery_control_write_all_at(&first.authority.lease, 0, &[0x22]).is_err()
        );
        assert!(first.validate_retained_authority().is_err());

        let mut displaced_control =
            File::open(displaced.join(ROOT_LEASE_NAME)).expect("open displaced recovery control");
        let mut first_byte = [0_u8; 1];
        displaced_control
            .read_exact(&mut first_byte)
            .expect("read displaced recovery control");
        assert_eq!(first_byte, [0x11]);

        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
        assert!(matches!(replacement.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn windows_exclusive_recovery_control_prevents_root_displacement() {
        let temporary = tempfile::tempdir().expect("temporary parent");
        let root = temporary.path().join("app");
        let displaced = temporary.path().join("displaced-app");
        std::fs::create_dir(&root).expect("application root");
        let session = acquire_test_root(&root);

        std::fs::rename(&root, &displaced)
            .expect_err("exclusive recovery control must prevent live root displacement");
        std::fs::create_dir(&root)
            .expect_err("occupied live root must prevent replacement creation");
        assert!(root.is_dir());
        assert!(!displaced.exists());
        platform::recovery_control_write_all_at(&session.authority.lease, 0, &[0x31])
            .expect("retained control write after denied root displacement");
        let mut byte = [0_u8; 1];
        platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte)
            .expect("retained control read after denied root displacement");
        assert_eq!(byte, [0x31]);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn recovery_control_initializes_only_zero_length_and_rejects_corrupt_lengths() {
        for (length, accepted) in [
            (0, true),
            (1, false),
            (recovery::RECOVERY_CONTROL_BYTES - 1, false),
            (recovery::RECOVERY_CONTROL_BYTES, true),
            (recovery::RECOVERY_CONTROL_BYTES + 1, false),
        ] {
            let temporary = tempfile::tempdir().expect("temporary root");
            let lease_path = temporary.path().join(ROOT_LEASE_NAME);
            File::create(&lease_path)
                .expect("create lease fixture")
                .set_len(length)
                .expect("size lease fixture");
            let outcome = RootSession::acquire(temporary.path());
            let expected = if length == 0 {
                recovery::RECOVERY_CONTROL_BYTES
            } else {
                length
            };
            assert_eq!(
                std::fs::metadata(&lease_path)
                    .expect("lease metadata")
                    .len(),
                expected,
                "initialization changed a nonzero corrupt length"
            );
            let session = if accepted {
                match outcome {
                    RootSessionAcquireOutcome::Acquired(session) => session,
                    other => panic!("valid recovery control was refused: {other:?}"),
                }
            } else {
                let obligation = match outcome {
                    RootSessionAcquireOutcome::AppliedUnverified(obligation)
                        if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
                    {
                        obligation
                    }
                    other => panic!("corrupt recovery control did not retain ownership: {other:?}"),
                };
                obligation
                    .acknowledge_preserved()
                    .expect("release corrupt recovery control ownership");
                File::options()
                    .write(true)
                    .open(&lease_path)
                    .expect("open corrupt recovery control")
                    .set_len(recovery::RECOVERY_CONTROL_BYTES)
                    .expect("repair recovery control length");
                acquire_test_root(temporary.path())
            };
            let mut last = [0xff_u8; 1];
            platform::recovery_control_read_exact_at(
                &session.authority.lease,
                recovery::RECOVERY_CONTROL_BYTES - 1,
                &mut last,
            )
            .expect("read exact recovery control tail");
            assert_eq!(last, [0]);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_exclusive_recovery_control_prevents_binding_substitution() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        let displaced = temporary.path().join("displaced-control");
        std::fs::rename(&lease, &displaced)
            .expect_err("exclusive retained control must prevent displacement");
        std::fs::remove_file(&lease).expect_err("exclusive retained control must prevent removal");
        File::create(&lease).expect_err("exclusive retained control must prevent substitution");

        platform::recovery_control_write_all_at(&session.authority.lease, 0, &[0x42])
            .expect("retained control write after denied substitution");
        let mut byte = [0_u8; 1];
        platform::recovery_control_read_exact_at(&session.authority.lease, 0, &mut byte)
            .expect("retained control read after denied substitution");
        assert_eq!(byte, [0x42]);
        assert!(!displaced.exists());
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(windows)]
    #[test]
    fn windows_recovery_control_reports_short_native_eof() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let hook =
            platform::install_recovery_control_read_test_hook(recovery::RECOVERY_CONTROL_BYTES - 1);
        let error = platform::recovery_control_read_exact_at(
            &session.authority.lease,
            recovery::RECOVERY_CONTROL_BYTES - 1,
            &mut [0_u8; 1],
        )
        .expect_err("short positional read must fail");
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        drop(hook);
        platform::set_recovery_control_len_for_test(
            &session.authority.lease,
            recovery::RECOVERY_CONTROL_BYTES,
        )
        .expect("repair retained control length");
        let mut repaired = [0xff_u8; 1];
        platform::recovery_control_read_exact_at(
            &session.authority.lease,
            recovery::RECOVERY_CONTROL_BYTES - 1,
            &mut repaired,
        )
        .expect("read repaired recovery control tail");
        assert_eq!(repaired, [0]);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn recovery_failure_preserves_root_artifacts_and_releases_the_lease() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let lease_path = temporary.path().join(ROOT_LEASE_NAME);
        let payload_path = temporary.path().join("owned.bin");
        let recovery_path = temporary
            .path()
            .join(".axial-rstage-11111111111111111111111111111111");
        std::fs::write(&lease_path, [0x7a]).expect("write corrupt recovery control");
        std::fs::write(&payload_path, b"owned payload").expect("write owned payload");
        std::fs::write(&recovery_path, b"recovery payload").expect("write recovery payload");

        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("corrupt recovery control did not retain ownership: {outcome:?}"),
        };
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        #[cfg(unix)]
        let obligation = {
            let lease = temporary.path().join(ROOT_LEASE_NAME);
            let displaced = temporary.path().join("displaced-replay-lease");
            std::fs::rename(&lease, &displaced).expect("displace retained replay lease");
            std::fs::write(&lease, b"replacement").expect("replace replay lease binding");
            let obligation = obligation
                .acknowledge_preserved()
                .expect_err("invalid replay lease must restore acquired ownership");
            std::fs::remove_file(&lease).expect("remove replacement replay lease");
            std::fs::rename(&displaced, &lease).expect("restore retained replay lease");
            obligation
        };
        let obligation = obligation
            .cleanup()
            .expect_err("recovery uncertainty must refuse destructive root cleanup");
        obligation
            .acknowledge_preserved()
            .expect("validated recovery failure can preserve its root");

        assert_eq!(std::fs::read(&lease_path).expect("read control"), [0x7a]);
        assert_eq!(
            std::fs::read(&payload_path).expect("read owned payload"),
            b"owned payload"
        );
        assert_eq!(
            std::fs::read(&recovery_path).expect("read recovery payload"),
            b"recovery payload"
        );
        let reacquired = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("preservation did not release the root lease: {outcome:?}"),
        };
        reacquired
            .acknowledge_preserved()
            .expect("settle the repeated recovery failure");
    }

    #[test]
    fn root_recovery_control_is_reserved_only_at_the_physical_root() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let nested_path = temporary.path().join("nested");
        std::fs::create_dir(&nested_path).expect("nested directory");
        std::fs::write(nested_path.join(ROOT_LEASE_NAME), b"user-owned").expect("nested user file");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let alternate = session
            .admit_absolute_directory(temporary.path())
            .expect("alternate root capability");
        let control = LeafName::new(ROOT_LEASE_NAME).expect("control leaf");
        assert_eq!(
            root.open_file(&control)
                .expect_err("root control must not become a file capability")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            alternate
                .open_file(&control)
                .expect_err("alternate root handle must not bypass control ownership")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        let nested = root
            .open_directory(&LeafName::new("nested").expect("nested leaf"))
            .expect("open nested directory");
        nested
            .open_file(&control)
            .expect("same spelling below root remains user-owned");
        drop((nested, alternate, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn bounded_recovery_stage_publishes_through_the_live_lifecycle() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("published.bin").expect("target leaf");
        let mut staged = match root.create_recoverable_stage(&target) {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
            FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
                FileCreateResolution::Created(staged) => staged,
                FileCreateResolution::NoEffect(error) => {
                    panic!("recovery create reconciled to no effect: {error}")
                }
                FileCreateResolution::Indeterminate(obligation) => {
                    panic!("recovery create remained unsettled: {}", obligation.error())
                }
            },
        };
        staged.write_all(b"durable payload").expect("write stage");
        let sealed = staged.seal().expect("seal recovery stage");
        let published = require_test_promotion(sealed.promote_no_replace(&root, &root, &target));
        assert_eq!(
            std::fs::read(temporary.path().join("published.bin")).expect("published payload"),
            b"durable payload"
        );
        drop((published, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn recoverable_state_stages_are_serialized_from_reservation_to_retirement() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let first_target = LeafName::new("first.json").expect("first target");
        let second_target = LeafName::new("second.json").expect("second target");
        let first = match root.create_recoverable_stage(&first_target) {
            FileCreateOutcome::Created(staged) => staged,
            outcome => panic!("first recovery stage was not created: {outcome:?}"),
        };
        match root.create_recoverable_stage(&second_target) {
            FileCreateOutcome::NoEffect(error) => {
                assert_eq!(error.kind(), io::ErrorKind::WouldBlock)
            }
            outcome => panic!("second recovery stage bypassed serialization: {outcome:?}"),
        }
        require_test_stage_discard(first.discard());
        let second = match root.create_recoverable_stage(&second_target) {
            FileCreateOutcome::Created(staged) => staged,
            outcome => panic!("retired recovery stage did not release serialization: {outcome:?}"),
        };
        require_test_stage_discard(second.discard());
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn live_state_successor_replaces_mixed_batch_in_input_order() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("state.json").expect("target leaf");
        let old = require_test_promotion(
            test_sealed_stage(&root, b"old State payload")
                .promote_no_replace(&root, &root, &target),
        );
        let old_revision = old.revision().expect("old revision");
        let old_digest = Sha256::digest(b"old State payload").into();
        let destination = ReplaceDestination::Existing(
            old.park_request(ExpectedFileContent::new(old_revision, old_digest)),
        );
        let vacant = ReplaceDestination::Vacant {
            parent: root.clone(),
            name: LeafName::new("vacant.json").expect("vacant target"),
        };
        let mut files = require_test_state_batch(root.replace_state_batch_durable(
            StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner"),
            vec![
                (vacant, b"vacant State payload".to_vec()),
                (destination, b"new State payload".to_vec()),
            ],
        ));
        assert_eq!(files.len(), 2);
        let current = files.pop().expect("existing State member");
        let vacant = files.pop().expect("vacant State member");
        assert_eq!(
            vacant.read_bounded(64).expect("read vacant State file"),
            b"vacant State payload"
        );
        assert_eq!(
            current.read_bounded(64).expect("read current State file"),
            b"new State payload"
        );
        {
            let operations = session
                .authority
                .operations
                .lock()
                .expect("operation state");
            assert!(!operations.recovery.is_uncertain());
            assert!(!operations.recovery.has_live_or_uncertain());
            assert!(operations.stages.is_empty());
        }
        drop((vacant, current, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn live_state_successor_reconciles_an_uncertain_create_before_handoff() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("state.json").expect("target leaf");
        let old = require_test_promotion(
            test_sealed_stage(&root, b"old State payload")
                .promote_no_replace(&root, &root, &target),
        );
        let revision = old.revision().expect("old revision");
        let digest = Sha256::digest(b"old State payload").into();
        let destination = ReplaceDestination::Existing(
            old.park_request(ExpectedFileContent::new(revision, digest)),
        );
        let failure = recovery::install_pre_barrier_sync_failure_after(1);
        let obligation = match root.replace_state_batch_durable(
            StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner"),
            vec![(destination, b"new State payload".to_vec())],
        ) {
            StateFileBatchOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("successor create uncertainty was not retained: {outcome:?}"),
        };
        drop(failure);
        let current = require_test_state_file(obligation.reconcile());
        assert_eq!(
            current.read_bounded(64).expect("read current State file"),
            b"new State payload"
        );
        drop((current, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn state_batch_second_member_failure_rolls_back_every_exact_input() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let destinations = ["first.json", "second.json"].map(|name| ReplaceDestination::Vacant {
            parent: root.clone(),
            name: LeafName::new(name).expect("batch target"),
        });
        let failure = recovery::install_pre_barrier_sync_failure_after(1);
        let outcome = root.replace_state_batch_durable(
            StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner"),
            destinations
                .into_iter()
                .zip([b"first".to_vec(), b"second".to_vec()])
                .collect(),
        );
        drop(failure);
        let replacements = match outcome {
            StateFileBatchOutcome::NoEffect { replacements, .. } => replacements,
            outcome => panic!("second member failure did not roll back: {outcome:?}"),
        };
        assert_eq!(
            replacements
                .iter()
                .map(|(_, contents)| contents.as_slice())
                .collect::<Vec<_>>(),
            [b"first".as_slice(), b"second".as_slice()]
        );
        assert!(!temporary.path().join("first.json").exists());
        assert!(!temporary.path().join("second.json").exists());
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert!(state.state_batch.is_none());
        assert!(state.recovery.records().next().is_none());
        drop((state, replacements, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn state_batch_partial_replay_retains_marker_and_retries_full_vector() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let replacements = ["first.json", "second.json"]
            .into_iter()
            .zip([b"first".to_vec(), b"second".to_vec()])
            .map(|(name, contents)| {
                (
                    ReplaceDestination::Vacant {
                        parent: root.clone(),
                        name: LeafName::new(name).expect("batch target"),
                    },
                    contents,
                )
            })
            .collect();
        let failure = recovery::install_pre_barrier_sync_failure_after(4);
        let obligation = match root.replace_state_batch_durable(
            StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner"),
            replacements,
        ) {
            StateFileBatchOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("partial replay did not retain its owner: {outcome:?}"),
        };
        drop(failure);
        assert!(
            session
                .authority
                .operations
                .lock()
                .expect("operation state")
                .state_batch
                .is_some()
        );
        let competing = root.replace_state_batch_durable(
            StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner"),
            vec![(
                ReplaceDestination::Vacant {
                    parent: root.clone(),
                    name: LeafName::new("competing.json").expect("competing target"),
                },
                b"competing".to_vec(),
            )],
        );
        assert!(matches!(competing, StateFileBatchOutcome::NoEffect { .. }));
        let files = require_test_state_batch(obligation.reconcile());
        assert_eq!(files.len(), 2);
        assert_eq!(
            std::fs::read(temporary.path().join("first.json")).expect("first target"),
            b"first"
        );
        assert_eq!(
            std::fs::read(temporary.path().join("second.json")).expect("second target"),
            b"second"
        );
        assert!(
            session
                .authority
                .operations
                .lock()
                .expect("operation state")
                .state_batch
                .is_none()
        );
        drop((files, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn state_batch_rejects_bounds_and_aliases_before_marker_or_io() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let request =
            || StateFileSuccessorRequest::new(1, b"state-test".as_slice()).expect("State owner");
        let no_effect = |outcome| match outcome {
            StateFileBatchOutcome::NoEffect { replacements, .. } => replacements,
            outcome => panic!("invalid State batch reached an effect: {outcome:?}"),
        };
        assert!(no_effect(root.replace_state_batch_durable(request(), Vec::new())).is_empty());

        let duplicate = || ReplaceDestination::Vacant {
            parent: root.clone(),
            name: LeafName::new("duplicate.json").expect("duplicate target"),
        };
        assert_eq!(
            no_effect(root.replace_state_batch_durable(
                request(),
                vec![
                    (duplicate(), Vec::new()),
                    (
                        ReplaceDestination::Vacant {
                            parent: root.clone(),
                            name: LeafName::new("DUPLICATE.json").expect("alias target"),
                        },
                        Vec::new(),
                    ),
                ],
            ))
            .len(),
            2
        );

        let excessive = (0..33)
            .map(|index| {
                (
                    ReplaceDestination::Vacant {
                        parent: root.clone(),
                        name: LeafName::new(format!("member-{index}.json"))
                            .expect("bounded target"),
                    },
                    Vec::new(),
                )
            })
            .collect();
        assert_eq!(
            no_effect(root.replace_state_batch_durable(request(), excessive)).len(),
            33
        );

        let oversized = vec![0_u8; recovery::MAX_RECOVERABLE_FILE_BYTES as usize + 1];
        assert_eq!(
            no_effect(root.replace_state_batch_durable(
                request(),
                vec![(
                    ReplaceDestination::Vacant {
                        parent: root.clone(),
                        name: LeafName::new("oversized.json").expect("oversized target"),
                    },
                    oversized,
                )],
            ))[0]
                .1
                .len(),
            recovery::MAX_RECOVERABLE_FILE_BYTES as usize + 1
        );
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert!(state.state_batch.is_none());
        assert!(state.recovery.records().next().is_none());
        drop((state, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn uncertain_recovery_seal_becomes_read_only_and_reseals_idempotently() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("uncertain-seal.bin").expect("target leaf");
        let mut staged = match root.create_recoverable_stage(&target) {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                panic!("recovery create was not verified: {}", obligation.error())
            }
        };
        staged
            .write_all(b"sealed despite uncertain sync")
            .expect("stage payload");
        let stage_path = temporary.path().join(staged.file.name.as_os_str());
        let hook = recovery::install_pre_barrier_sync_failure();
        let failure = staged
            .seal()
            .expect_err("injected recovery sync failure must retain the stage");
        drop(hook);
        assert!(
            session
                .authority
                .operations
                .lock()
                .expect("recovery state")
                .recovery
                .is_uncertain()
        );

        let mut staged = failure.into_staged();
        assert_eq!(
            staged
                .writer()
                .expect_err("durably sealed carrier must not become writable")
                .kind(),
            io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            std::fs::read(&stage_path).expect("read unchanged stage"),
            b"sealed despite uncertain sync"
        );
        let sealed = staged
            .seal()
            .expect("exact reconciled seal must be idempotent");
        require_test_stage_discard(sealed.discard());
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn sealed_recovery_reserves_target_until_durable_cancellation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("reserved-target.bin").expect("target leaf");
        let (recovery, _) = test_recoverable_stage(&root, &target, b"recovery payload");
        let ordinary = test_sealed_stage(&root, b"ordinary payload");

        let ordinary =
            require_test_promotion_no_effect(ordinary.promote_no_replace(&root, &root, &target));
        require_test_stage_discard(recovery.discard());
        let published = require_test_promotion(ordinary.promote_no_replace(&root, &root, &target));
        assert_eq!(
            published.read_bounded(32).expect("published payload"),
            b"ordinary payload"
        );
        drop((published, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn recovery_seal_refuses_an_existing_target_reservation() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("reserved-before-seal.bin").expect("target leaf");
        let ordinary = test_sealed_stage(&root, b"ordinary payload");
        let (attempt_id, receipt) = test_publication_attempt(&ordinary, &root, &target);
        ordinary
            .token
            .prepare_promotion(&root, &target, attempt_id, receipt, None, None)
            .expect("reserve ordinary publication target");

        let mut recovery = match root.create_recoverable_stage(&target) {
            FileCreateOutcome::Created(staged) => staged,
            FileCreateOutcome::NoEffect(error) => panic!("recovery create failed: {error}"),
            FileCreateOutcome::AppliedUnverified(obligation) => {
                panic!("recovery create was not verified: {}", obligation.error())
            }
        };
        recovery
            .write_all(b"recovery payload")
            .expect("recovery payload");
        let failure = recovery
            .seal()
            .expect_err("existing target reservation must block durable seal");
        ordinary
            .token
            .update(StageRegistryPhase::Sealed)
            .expect("release ordinary publication reservation");
        let sealed = failure
            .into_staged()
            .seal()
            .expect("recovery seal after reservation release");
        require_test_stage_discard(sealed.discard());
        require_test_stage_discard(ordinary.discard());
        drop(root);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn prepared_recovery_replay_ignores_user_target_and_create_only_park() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let operation_id = [0x71; 16];
        let (registration, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            operation_id,
            "user-target.bin",
            RecoveryPhase::StagePrepared,
            b"unsealed stage",
            true,
        );
        let park = recovery_park_leaf(operation_id);
        std::fs::write(temporary.path().join("user-target.bin"), b"user target")
            .expect("user target");
        std::fs::write(temporary.path().join(park.as_str()), b"user park").expect("user park");
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert_eq!(
            std::fs::read(temporary.path().join("user-target.bin")).expect("read user target"),
            b"user target"
        );
        assert_eq!(
            std::fs::read(temporary.path().join(park.as_str())).expect("read user park"),
            b"user park"
        );
        let root = replayed.root().expect("replayed root");
        let target_file = root
            .open_file(&LeafName::new("user-target.bin").expect("target leaf"))
            .expect("prepared recovery does not reserve target");
        let park_file = root
            .open_file(&LeafName::new(park.as_str()).expect("park leaf"))
            .expect("create-only recovery does not reserve park");
        {
            let state = replayed.authority.operations.lock().expect("replay state");
            #[cfg(unix)]
            assert!(state.recovery.record(registration).is_none());
            #[cfg(windows)]
            {
                assert_eq!(
                    state
                        .recovery
                        .record(registration)
                        .expect("retained prepared recovery")
                        .phase,
                    RecoveryPhase::StagePrepared
                );
                let orphan = state
                    .recovery_orphans
                    .iter()
                    .find(|orphan| orphan.registration == registration)
                    .expect("prepared recovery orphan");
                assert!(orphan.files.is_empty());
            }
        }
        drop((target_file, park_file, root));
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replay_cancels_sealed_and_unpublished_prepared_stages_on_target_collision() {
        for (phase, operation_byte) in [
            (RecoveryPhase::StageSealed, 0x72),
            (RecoveryPhase::PublishPrepared, 0x73),
        ] {
            let temporary = tempfile::tempdir().expect("temporary root");
            let first = acquire_test_root(temporary.path());
            let (registration, stage) = persist_test_recovery_fixture(
                &first,
                temporary.path(),
                [operation_byte; 16],
                "occupied.bin",
                phase,
                b"owned stage",
                true,
            );
            std::fs::write(temporary.path().join("occupied.bin"), b"unrelated target")
                .expect("unrelated target");
            assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

            let replayed = acquire_test_root(temporary.path());
            assert!(!temporary.path().join(stage.as_str()).exists());
            assert_eq!(
                std::fs::read(temporary.path().join("occupied.bin"))
                    .expect("read unrelated target"),
                b"unrelated target"
            );
            let root = replayed.root().expect("replayed root");
            let target = root
                .open_file(&LeafName::new("occupied.bin").expect("target leaf"))
                .expect("cancelled recovery does not reserve target");
            {
                let state = replayed.authority.operations.lock().expect("replay state");
                #[cfg(unix)]
                {
                    assert!(state.recovery.record(registration).is_none());
                    assert!(
                        state
                            .recovery_orphans
                            .iter()
                            .all(|orphan| orphan.registration != registration)
                    );
                }
                #[cfg(windows)]
                {
                    assert_eq!(
                        state
                            .recovery
                            .record(registration)
                            .expect("retained cancellation")
                            .phase,
                        RecoveryPhase::RemovePrepared
                    );
                    let orphan = state
                        .recovery_orphans
                        .iter()
                        .find(|orphan| orphan.registration == registration)
                        .expect("cancellation orphan");
                    assert!(orphan.files.is_empty());
                }
            }
            drop((target, root));
            assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
        }
    }

    #[cfg(unix)]
    #[test]
    fn sealed_recovery_without_its_carrier_fails_closed_and_preserves_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let (_, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x74; 16],
            "unrelated.bin",
            RecoveryPhase::StageSealed,
            b"same bytes",
            false,
        );
        std::fs::write(temporary.path().join("unrelated.bin"), b"same bytes")
            .expect("unrelated target");
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("missing sealed carrier did not fail closed: {outcome:?}"),
        };
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert_eq!(
            std::fs::read(temporary.path().join("unrelated.bin"))
                .expect("preserved unrelated target"),
            b"same bytes"
        );
        obligation
            .acknowledge_preserved()
            .expect("preserve failed recovery root");
    }

    #[test]
    fn recoverable_discard_releases_target_in_the_same_session() {
        for publish_prepared in [false, true] {
            let temporary = tempfile::tempdir().expect("temporary root");
            let session = acquire_test_root(temporary.path());
            let root = session.root().expect("root capability");
            let target = LeafName::new("after-cancel.bin").expect("target leaf");
            if !publish_prepared {
                std::fs::write(
                    temporary.path().join("after-cancel.bin"),
                    b"preexisting user target",
                )
                .expect("preexisting user target");
            }
            let (staged, registration) = test_recoverable_stage(&root, &target, b"owned payload");
            let staged = if publish_prepared {
                std::fs::write(
                    temporary.path().join("after-cancel.bin"),
                    b"unrelated target",
                )
                .expect("unrelated target");
                let staged = require_test_promotion_no_effect(
                    staged.promote_no_replace(&root, &root, &target),
                );
                assert_eq!(
                    session
                        .authority
                        .operations
                        .lock()
                        .expect("recovery state")
                        .recovery
                        .record(registration)
                        .expect("publish-prepared record")
                        .phase,
                    RecoveryPhase::PublishPrepared
                );
                staged
            } else {
                staged
            };
            require_test_stage_discard(staged.discard());
            if !publish_prepared {
                std::fs::write(
                    temporary.path().join("after-cancel.bin"),
                    b"new user target",
                )
                .expect("new user target");
            }
            let opened = root
                .open_file(&target)
                .expect("cancelled recovery releases target coordinate");
            {
                let state = session.authority.operations.lock().expect("recovery state");
                #[cfg(unix)]
                assert!(state.recovery.record(registration).is_none());
                #[cfg(windows)]
                {
                    assert_eq!(
                        state
                            .recovery
                            .record(registration)
                            .expect("retained cancellation")
                            .phase,
                        RecoveryPhase::RemovePrepared
                    );
                    let orphan = state
                        .recovery_orphans
                        .iter()
                        .find(|orphan| orphan.registration == registration)
                        .expect("cancellation orphan");
                    assert!(orphan.files.is_empty());
                }
            }
            drop((opened, root));
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
        }
    }

    #[test]
    fn publish_prepared_no_effect_can_retry_exactly() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("retried.bin").expect("target leaf");
        let (staged, registration) = test_recoverable_stage(&root, &target, b"retry payload");
        std::fs::write(temporary.path().join("retried.bin"), b"collision")
            .expect("promotion collision");
        let staged =
            require_test_promotion_no_effect(staged.promote_no_replace(&root, &root, &target));
        assert_eq!(
            session
                .authority
                .operations
                .lock()
                .expect("recovery state")
                .recovery
                .record(registration)
                .expect("publish-prepared record")
                .phase,
            RecoveryPhase::PublishPrepared
        );
        std::fs::remove_file(temporary.path().join("retried.bin"))
            .expect("remove promotion collision");

        let published = require_test_promotion(staged.promote_no_replace(&root, &root, &target));
        assert_eq!(
            std::fs::read(temporary.path().join("retried.bin")).expect("retried target"),
            b"retry payload"
        );
        {
            let state = session.authority.operations.lock().expect("recovery state");
            #[cfg(unix)]
            assert!(state.recovery.record(registration).is_none());
            #[cfg(windows)]
            {
                assert_eq!(
                    state
                        .recovery
                        .record(registration)
                        .expect("committed publication")
                        .phase,
                    RecoveryPhase::RemoveCommitted
                );
                let orphan = state
                    .recovery_orphans
                    .iter()
                    .find(|orphan| orphan.registration == registration)
                    .expect("publication orphan");
                assert!(orphan.files.as_slice() == [published.identity]);
            }
        }
        drop((published, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn reconciled_applied_publication_completes_recovery_before_disarm() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let target = LeafName::new("reconciled.bin").expect("target leaf");
        let (staged, registration) = test_recoverable_stage(&root, &target, b"reconciled payload");
        let authority = root.authority().expect("recovery authority");
        let operation = authority.enter().expect("publication operation");
        let expected = validate_recovery_publication(
            &authority,
            &operation,
            &staged.token,
            &staged.file,
            &staged.revision,
            &root,
            &root,
            &target,
            None,
        )
        .expect("validate recoverable publication");
        let (attempt_id, mut receipt) = test_publication_attempt(&staged, &root, &target);
        staged
            .token
            .prepare_promotion(
                &root,
                &target,
                attempt_id,
                receipt.clone(),
                None,
                expected.as_ref(),
            )
            .expect("prepare recoverable publication");
        platform::rename_no_replace(
            &mut receipt,
            attempt_id,
            &root.inner.handle,
            staged.file.name.as_os_str(),
            &staged.file.handle,
            &root.inner.handle,
            target.as_os_str(),
        )
        .expect("apply publication before reconciliation");
        staged.token.record_publication(attempt_id, receipt.clone());
        drop(operation);
        let resolution = FilePromotionObligation {
            error: io::Error::other("test publication requires reconciliation"),
            retained: staged,
            destination: root.clone(),
            destination_name: target.clone(),
            attempt_id,
            receipt,
        }
        .reconcile();
        let published = match resolution {
            FilePromotionResolution::Applied(file) => file,
            FilePromotionResolution::NoEffect(staged) => {
                require_test_stage_discard(staged.discard());
                panic!("applied publication reconciled to no effect")
            }
            FilePromotionResolution::Indeterminate(obligation) => panic!(
                "applied publication remained indeterminate: {}",
                obligation.error()
            ),
        };
        assert_eq!(
            std::fs::read(temporary.path().join("reconciled.bin")).expect("reconciled publication"),
            b"reconciled payload"
        );
        {
            let state = session.authority.operations.lock().expect("recovery state");
            assert!(state.stages.is_empty());
            #[cfg(unix)]
            assert!(state.recovery.record(registration).is_none());
            #[cfg(windows)]
            {
                assert_eq!(
                    state
                        .recovery
                        .record(registration)
                        .expect("committed publication")
                        .phase,
                    RecoveryPhase::RemoveCommitted
                );
                let orphan = state
                    .recovery_orphans
                    .iter()
                    .find(|orphan| orphan.registration == registration)
                    .expect("publication orphan");
                assert!(orphan.files.as_slice() == [published.identity]);
            }
        }
        drop((published, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn uncertain_startup_replay_retains_exclusive_stage_until_retry() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let operation_id = [0x5a; 16];
        let payload = b"restart payload";
        let (_, stage_name) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            operation_id,
            "replayed.bin",
            RecoveryPhase::StageSealed,
            payload,
            true,
        );
        let stage_path = temporary.path().join(stage_name.as_str());
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        recovery_runtime::fail_next_replay_exclusive_admission();
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("uncertain startup replay did not retain ownership: {outcome:?}"),
        };
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        let obligation = obligation
            .cleanup()
            .expect_err("acquired replay ownership must refuse cleanup");
        let phase_failure = recovery::install_pre_barrier_sync_failure();
        let obligation = match obligation.reconcile() {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("publication phase did not retain replay: {outcome:?}"),
        };
        drop(phase_failure);
        let settlement_failure = recovery::install_pre_barrier_sync_failure();
        let obligation = match obligation.reconcile() {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("publication settlement did not retain replay: {outcome:?}"),
        };
        drop(settlement_failure);
        let replayed = match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("settled startup replay did not reconcile: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(temporary.path().join("replayed.bin")).expect("replayed target"),
            payload
        );
        assert!(!stage_path.exists());
        #[cfg(unix)]
        assert_eq!(
            recovery_runtime::take_replay_tombstone_prunes(),
            1,
            "confirmed tombstone must retire the exclusive carrier after receipt settlement",
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replay_transfer_preflight_failure_moves_neither_of_two_carriers() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        for (operation, destination, payload) in [
            (
                [0x5b; 16],
                "first-replayed.bin",
                b"first payload".as_slice(),
            ),
            (
                [0x5c; 16],
                "second-replayed.bin",
                b"second payload".as_slice(),
            ),
        ] {
            persist_test_recovery_fixture(
                &first,
                temporary.path(),
                operation,
                destination,
                RecoveryPhase::StageSealed,
                payload,
                true,
            );
        }
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let phase_failure = recovery::install_pre_barrier_sync_failure();
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("initial replay did not retain both carriers: {outcome:?}"),
        };
        drop(phase_failure);
        assert_eq!(recovery_runtime::take_replay_transfer_commits(), 0);

        recovery_runtime::fail_replay_transfer_after(2);
        let obligation = match obligation.reconcile() {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("second transfer mapping did not fail in preflight: {outcome:?}"),
        };
        assert_eq!(
            recovery_runtime::take_replay_transfer_commits(),
            0,
            "mapping two failed after validation, so mapping one must not move",
        );

        let replayed = match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("unchanged retained carriers did not replay: {outcome:?}"),
        };
        assert_eq!(recovery_runtime::take_replay_transfer_commits(), 2);
        assert_eq!(
            std::fs::read(temporary.path().join("first-replayed.bin"))
                .expect("first replayed target"),
            b"first payload",
        );
        assert_eq!(
            std::fs::read(temporary.path().join("second-replayed.bin"))
                .expect("second replayed target"),
            b"second payload",
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replacement_replay_completes_the_full_commit_sequence() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"old replacement payload".as_slice();
        let new_payload = b"new replacement payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x81; 16],
            "replacement.bin",
            RecoveryPhase::StageSealed,
            old_payload,
            new_payload,
            [Some(new_payload), Some(old_payload), None],
        );
        let stage_path = temporary.path().join(stage.as_str());
        let park_path = temporary.path().join(park.as_str());
        let stage_identity = test_file_identity(&stage_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        let target_path = temporary.path().join("replacement.bin");
        assert_eq!(
            std::fs::read(&target_path).expect("replacement target"),
            new_payload
        );
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(!stage_path.exists());
        assert!(!park_path.exists());
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[stage_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn admitted_state_successor_replays_before_root_session_exposure() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let payload = b"State successor payload";
        let (registration, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x80; 16],
            "operation-journals.json",
            RecoveryPhase::StagePrepared,
            payload,
            true,
        );
        persist_test_state_successor(&first, registration, payload);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("live State successor did not retain acquisition: {outcome:?}"),
        };
        let successor = obligation
            .state_successor()
            .expect("inspect State successor")
            .expect("State successor admission");
        assert_eq!(successor.owner_schema(), 1);
        assert_eq!(successor.owner_id(), b"operation-journals");
        assert_eq!(successor.recovery_count(), 1);
        assert_eq!(
            successor.recovery_destination(0),
            Some((Vec::new(), "operation-journals.json"))
        );

        let replayed = match obligation.reconcile_state_successor(successor) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("admitted State successor did not replay: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(temporary.path().join("operation-journals.json"))
                .expect("replayed State target"),
            payload
        );
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(
            !replayed
                .authority
                .operations
                .lock()
                .expect("replayed recovery state")
                .recovery
                .has_live_successor()
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn cold_state_batch_replays_every_member_after_partial_physical_clear() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let first_payload = b"first cold State payload";
        let second_payload = b"second cold State payload";
        let parent = ["performance", "operations"]
            .map(|component| RecoveryName::new_exact(component).expect("recovery parent"))
            .to_vec();
        let first_leaf = "11111111-1111-4111-8111-111111111111.json";
        let second_leaf = "22222222-2222-4222-8222-222222222222.json";
        std::fs::create_dir_all(temporary.path().join("performance/operations"))
            .expect("performance operation directory");
        let mut first_operation = [0x11; 16];
        first_operation[6] = 0x41;
        first_operation[8] = 0x81;
        let mut second_operation = [0x22; 16];
        second_operation[6] = 0x42;
        second_operation[8] = 0x82;
        let (first_registration, first_stage) = persist_test_recovery_fixture_in(
            &first,
            temporary.path(),
            first_operation,
            parent.clone(),
            first_leaf,
            RecoveryPhase::StagePrepared,
            first_payload,
            true,
        );
        let (second_registration, second_stage) = persist_test_recovery_fixture_in(
            &first,
            temporary.path(),
            second_operation,
            parent,
            second_leaf,
            RecoveryPhase::StagePrepared,
            second_payload,
            true,
        );
        std::fs::rename(
            temporary
                .path()
                .join("performance/operations")
                .join(first_stage.as_str()),
            temporary
                .path()
                .join("performance/operations")
                .join(first_leaf),
        )
        .expect("publish first member before its physical clear");
        persist_test_state_successor_batch_with_owner(
            &first,
            b"performance-operation",
            &[
                (first_registration, first_payload.as_slice()),
                (second_registration, second_payload.as_slice()),
            ],
            &[first_registration],
        );
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("cold State batch did not retain acquisition: {outcome:?}"),
        };
        let successor = obligation
            .state_successor()
            .expect("inspect State batch")
            .expect("State batch admission");
        assert_eq!(successor.recovery_count(), 2);
        assert_eq!(
            successor.recovery_destination(0),
            Some((vec!["performance", "operations"], first_leaf))
        );
        assert_eq!(
            successor.recovery_destination(1),
            Some((vec!["performance", "operations"], second_leaf))
        );
        let replayed = match obligation.reconcile_state_successor(successor) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("cold State batch did not replay: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(
                temporary
                    .path()
                    .join("performance/operations")
                    .join(first_leaf)
            )
            .expect("first target"),
            first_payload
        );
        assert_eq!(
            std::fs::read(
                temporary
                    .path()
                    .join("performance/operations")
                    .join(second_leaf)
            )
            .expect("second target"),
            second_payload
        );
        assert!(
            !temporary
                .path()
                .join("performance/operations")
                .join(first_stage.as_str())
                .exists()
        );
        assert!(
            !temporary
                .path()
                .join("performance/operations")
                .join(second_stage.as_str())
                .exists()
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn state_successor_retains_completed_effects_until_tombstone_retry() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let payload = b"retryable State successor payload";
        let (registration, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x81; 16],
            "operation-journals.json",
            RecoveryPhase::StagePrepared,
            payload,
            true,
        );
        persist_test_state_successor(&first, registration, payload);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("live State successor did not retain acquisition: {outcome:?}"),
        };
        let successor = obligation
            .state_successor()
            .expect("inspect State successor")
            .expect("State successor admission");
        let sync_failure = recovery::install_pre_barrier_sync_failure();
        let obligation = match obligation.reconcile_state_successor(successor) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("successor tombstone failure did not retain replay: {outcome:?}"),
        };
        drop(sync_failure);
        assert_eq!(
            std::fs::read(temporary.path().join("operation-journals.json"))
                .expect("published State target"),
            payload
        );
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        let obligation = obligation
            .cleanup()
            .expect_err("armed successor replay must refuse cleanup");
        let successor = obligation
            .state_successor()
            .expect("reinspect State successor")
            .expect("retained State successor admission");
        let replayed = match obligation.reconcile_state_successor(successor) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("State successor tombstone did not reconcile: {outcome:?}"),
        };
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn state_successor_admission_cannot_cross_root_lineage() {
        fn prepare(path: &Path, operation_id: [u8; 16]) -> RootSessionAcquireObligation {
            let session = acquire_test_root(path);
            let payload = b"lineage-bound State payload";
            let (registration, _) = persist_test_recovery_fixture(
                &session,
                path,
                operation_id,
                "operation-journals.json",
                RecoveryPhase::StagePrepared,
                payload,
                true,
            );
            persist_test_state_successor(&session, registration, payload);
            assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
            match RootSession::acquire(path) {
                RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
                outcome => panic!("State successor did not retain acquisition: {outcome:?}"),
            }
        }

        let first = tempfile::tempdir().expect("first temporary root");
        let second = tempfile::tempdir().expect("second temporary root");
        let first_obligation = prepare(first.path(), [0x91; 16]);
        let second_obligation = prepare(second.path(), [0x92; 16]);
        let first_token = first_obligation
            .state_successor()
            .expect("inspect first successor")
            .expect("first successor token");
        let second_token = second_obligation
            .state_successor()
            .expect("inspect second successor")
            .expect("second successor token");
        drop(first_token);

        let first_obligation = match first_obligation.reconcile_state_successor(second_token) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation) => obligation,
            outcome => panic!("cross-root successor token was accepted: {outcome:?}"),
        };
        let first_token = first_obligation
            .state_successor()
            .expect("reinspect first successor")
            .expect("restored first successor token");
        let first_session = match first_obligation.reconcile_state_successor(first_token) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("first successor did not recover: {outcome:?}"),
        };
        let second_token = second_obligation
            .state_successor()
            .expect("reinspect second successor")
            .expect("restored second successor token");
        let second_session = match second_obligation.reconcile_state_successor(second_token) {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("second successor did not recover: {outcome:?}"),
        };
        assert!(matches!(first_session.revoke(), RootRevokeOutcome::Revoked));
        assert!(matches!(
            second_session.revoke(),
            RootRevokeOutcome::Revoked
        ));
    }

    #[test]
    fn replacement_replay_cancels_without_touching_a_foreign_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"expected old payload".as_slice();
        let new_payload = b"cancelled replacement payload".as_slice();
        let foreign_payload = b"unrelated target payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x82; 16],
            "foreign.bin",
            RecoveryPhase::StageSealed,
            old_payload,
            new_payload,
            [Some(new_payload), Some(foreign_payload), None],
        );
        let target_path = temporary.path().join("foreign.bin");
        let foreign_identity = test_file_identity(&target_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        assert_eq!(
            std::fs::read(&target_path).expect("foreign target"),
            foreign_payload
        );
        assert!(test_file_identity(&target_path) == foreign_identity);
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(!temporary.path().join(park.as_str()).exists());
        assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::RemovePrepared, &[]);
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replacement_prepared_stage_cleanup_ignores_the_user_target() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"expected old payload".as_slice();
        let staged_payload = b"unsealed replacement payload".as_slice();
        let user_payload = b"user target payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x89; 16],
            "prepared-user-target.bin",
            RecoveryPhase::StagePrepared,
            old_payload,
            staged_payload,
            [Some(staged_payload), Some(user_payload), None],
        );
        let target_path = temporary.path().join("prepared-user-target.bin");
        let user_identity = test_file_identity(&target_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        assert_eq!(
            std::fs::read(&target_path).expect("preserved user target"),
            user_payload
        );
        assert!(test_file_identity(&target_path) == user_identity);
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(!temporary.path().join(park.as_str()).exists());
        assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::StagePrepared, &[]);
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replacement_replay_restores_an_interrupted_park() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"restored old payload".as_slice();
        let new_payload = b"unpublished replacement payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x83; 16],
            "restored.bin",
            RecoveryPhase::ReplacePrepared,
            old_payload,
            new_payload,
            [None, None, Some(old_payload)],
        );
        let park_path = temporary.path().join(park.as_str());
        let park_identity = test_file_identity(&park_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        let target_path = temporary.path().join("restored.bin");
        assert_eq!(
            std::fs::read(&target_path).expect("restored target"),
            old_payload
        );
        assert!(test_file_identity(&target_path) == park_identity);
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(!park_path.exists());
        assert_test_recovery_terminal(&replayed, registration, RecoveryPhase::RemovePrepared, &[]);
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn equal_proof_replacement_prefers_the_stage_carrier() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let payload = b"equal proof payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x84; 16],
            "equal.bin",
            RecoveryPhase::StageSealed,
            payload,
            payload,
            [Some(payload), Some(payload), None],
        );
        let stage_path = temporary.path().join(stage.as_str());
        let target_path = temporary.path().join("equal.bin");
        let stage_identity = test_file_identity(&stage_path);
        let old_target_identity = test_file_identity(&target_path);
        assert!(stage_identity != old_target_identity);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        assert_eq!(std::fs::read(&target_path).expect("equal target"), payload);
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(test_file_identity(&target_path) != old_target_identity);
        assert!(!stage_path.exists());
        assert!(!temporary.path().join(park.as_str()).exists());
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[stage_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn replacement_replay_retries_settlement_after_park_removal() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"removed park payload".as_slice();
        let new_payload = b"already published payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x85; 16],
            "settlement-retry.bin",
            RecoveryPhase::PublishPrepared,
            old_payload,
            new_payload,
            [None, Some(new_payload), Some(old_payload)],
        );
        let target_path = temporary.path().join("settlement-retry.bin");
        let target_identity = test_file_identity(&target_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        recovery_runtime::fail_next_replay_removal_settlement();
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("post-removal settlement did not retain replay: {outcome:?}"),
        };
        assert_eq!(
            std::fs::read(&target_path).expect("retained published target"),
            new_payload
        );
        assert!(test_file_identity(&target_path) == target_identity);
        assert!(!temporary.path().join(stage.as_str()).exists());
        assert!(!temporary.path().join(park.as_str()).exists());
        assert!(matches!(
            RootSession::acquire(temporary.path()),
            RootSessionAcquireOutcome::NoEffect(RootSessionError::Busy)
        ));
        assert_eq!(
            recovery_runtime::take_replay_pending_removal_settlements(),
            0,
            "the injected cut must occur before pending settlement starts",
        );
        let obligation = obligation
            .cleanup()
            .expect_err("armed replacement replay must refuse cleanup");

        let replayed = match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("post-removal settlement did not reconcile: {outcome:?}"),
        };
        assert_eq!(
            recovery_runtime::take_replay_pending_removal_settlements(),
            1,
            "retry must settle the typed removed carrier before classification",
        );
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[target_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn replacement_publication_retries_a_poisoned_recovery_receipt() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"parked publication payload".as_slice();
        let new_payload = b"published after retry payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x8a; 16],
            "poisoned-retry.bin",
            RecoveryPhase::PublishPrepared,
            old_payload,
            new_payload,
            [Some(new_payload), None, Some(old_payload)],
        );
        let stage_path = temporary.path().join(stage.as_str());
        let target_path = temporary.path().join("poisoned-retry.bin");
        let park_path = temporary.path().join(park.as_str());
        let stage_identity = test_file_identity(&stage_path);
        let park_identity = test_file_identity(&park_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let first_failure =
            platform::install_publication_directory_sync_test_outcomes([Err(io::ErrorKind::Other)]);
        let obligation = match RootSession::acquire(temporary.path()) {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("publication barrier did not retain replay: {outcome:?}"),
        };
        drop(first_failure);
        assert!(!stage_path.exists());
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(test_file_identity(&park_path) == park_identity);

        let second_failure =
            platform::install_publication_directory_sync_test_outcomes([Err(io::ErrorKind::Other)]);
        let obligation = match obligation.reconcile() {
            RootSessionAcquireOutcome::AppliedUnverified(obligation)
                if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
            {
                obligation
            }
            outcome => panic!("poisoned publication receipt was not retryable: {outcome:?}"),
        };
        drop(second_failure);
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(test_file_identity(&park_path) == park_identity);

        let successful_barriers =
            platform::install_publication_directory_sync_test_outcomes([Ok(()), Ok(())]);
        let replayed = match obligation.reconcile() {
            RootSessionAcquireOutcome::Acquired(session) => session,
            outcome => panic!("poisoned publication receipt did not settle: {outcome:?}"),
        };
        drop(successful_barriers);
        assert_eq!(
            std::fs::read(&target_path).expect("settled replacement target"),
            new_payload
        );
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(!park_path.exists());
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[stage_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn remove_committed_replay_finishes_the_durable_desired_state() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let old_payload = b"durably superseded payload".as_slice();
        let new_payload = b"durable desired payload".as_slice();
        let (registration, stage, park) = persist_test_replacement_fixture(
            &first,
            temporary.path(),
            [0x86; 16],
            "durable-desired.bin",
            RecoveryPhase::RemoveCommitted,
            old_payload,
            new_payload,
            [Some(new_payload), Some(old_payload), None],
        );
        let stage_path = temporary.path().join(stage.as_str());
        let stage_identity = test_file_identity(&stage_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        let target_path = temporary.path().join("durable-desired.bin");
        assert_eq!(
            std::fs::read(&target_path).expect("durable desired target"),
            new_payload
        );
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(!stage_path.exists());
        assert!(!temporary.path().join(park.as_str()).exists());
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[stage_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn create_only_remove_committed_republishes_its_stage() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let payload = b"create-only durable payload".as_slice();
        let (registration, stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x8b; 16],
            "create-only-durable.bin",
            RecoveryPhase::RemoveCommitted,
            payload,
            true,
        );
        let stage_path = temporary.path().join(stage.as_str());
        let stage_identity = test_file_identity(&stage_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        let target_path = temporary.path().join("create-only-durable.bin");
        assert_eq!(
            std::fs::read(&target_path).expect("create-only durable target"),
            payload
        );
        assert!(test_file_identity(&target_path) == stage_identity);
        assert!(!stage_path.exists());
        assert_test_recovery_terminal(
            &replayed,
            registration,
            RecoveryPhase::RemoveCommitted,
            &[stage_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn create_only_and_replacement_replay_share_one_parent_snapshot() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let first = acquire_test_root(temporary.path());
        let create_payload = b"create-only payload".as_slice();
        let (create_registration, create_stage) = persist_test_recovery_fixture(
            &first,
            temporary.path(),
            [0x87; 16],
            "created.bin",
            RecoveryPhase::StageSealed,
            create_payload,
            true,
        );
        let old_payload = b"mixed old payload".as_slice();
        let new_payload = b"mixed replacement payload".as_slice();
        let (replacement_registration, replacement_stage, replacement_park) =
            persist_test_replacement_fixture(
                &first,
                temporary.path(),
                [0x88; 16],
                "mixed-replacement.bin",
                RecoveryPhase::StageSealed,
                old_payload,
                new_payload,
                [Some(new_payload), Some(old_payload), None],
            );
        let create_stage_path = temporary.path().join(create_stage.as_str());
        let replacement_stage_path = temporary.path().join(replacement_stage.as_str());
        let create_identity = test_file_identity(&create_stage_path);
        let replacement_identity = test_file_identity(&replacement_stage_path);
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let replayed = acquire_test_root(temporary.path());
        let created_path = temporary.path().join("created.bin");
        let replacement_path = temporary.path().join("mixed-replacement.bin");
        assert_eq!(
            std::fs::read(&created_path).expect("created target"),
            create_payload
        );
        assert_eq!(
            std::fs::read(&replacement_path).expect("replacement target"),
            new_payload
        );
        assert!(test_file_identity(&created_path) == create_identity);
        assert!(test_file_identity(&replacement_path) == replacement_identity);
        assert!(!create_stage_path.exists());
        assert!(!replacement_stage_path.exists());
        assert!(!temporary.path().join(replacement_park.as_str()).exists());
        assert_test_recovery_terminal(
            &replayed,
            create_registration,
            RecoveryPhase::RemoveCommitted,
            &[create_identity],
        );
        assert_test_recovery_terminal(
            &replayed,
            replacement_registration,
            RecoveryPhase::RemoveCommitted,
            &[replacement_identity],
        );
        assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn unresolved_recovery_replay_drop_aborts() {
        const CHILD: &str = "AXIAL_TEST_DROP_ARMED_RECOVERY_REPLAY";
        if std::env::var_os(CHILD).is_some() {
            let temporary = tempfile::tempdir().expect("temporary root");
            let first = acquire_test_root(temporary.path());
            persist_test_recovery_fixture(
                &first,
                temporary.path(),
                [0x5d; 16],
                "unresolved.bin",
                RecoveryPhase::StageSealed,
                b"unresolved payload",
                true,
            );
            assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));
            recovery_runtime::fail_next_replay_exclusive_admission();
            let obligation = match RootSession::acquire(temporary.path()) {
                RootSessionAcquireOutcome::AppliedUnverified(obligation)
                    if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
                {
                    obligation
                }
                outcome => panic!("child did not acquire armed replay ownership: {outcome:?}"),
            };
            drop(obligation);
            panic!("dropping armed replay ownership returned");
        }

        use std::os::unix::process::ExitStatusExt as _;
        let status = std::process::Command::new(std::env::current_exe().expect("test executable"))
            .arg("unresolved_recovery_replay_drop_aborts")
            .arg("--nocapture")
            .env(CHILD, "1")
            .status()
            .expect("run armed replay drop child");
        assert_eq!(status.signal(), Some(6));
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn replay_parent_swap_after_planning_retains_the_record_until_binding_restoration() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let destination = temporary.path().join("destination").join("nested");
        let displaced = temporary.path().join("displaced-nested-destination");
        std::fs::create_dir(temporary.path().join("destination"))
            .expect("outer recovery destination");
        std::fs::create_dir(&destination).expect("nested recovery destination");
        let first = acquire_test_root(temporary.path());
        let operation_id = [0x6b; 16];
        let stage_name = recovery::recovery_stage_leaf(operation_id);
        let target = recovery::RecoveryName::new_exact("replayed.bin").expect("target name");
        let payload = b"parent-bound restart payload";
        let mut journal = recovery::RecoveryJournal::load(&first.authority.lease)
            .expect("load independent recovery fixture journal");
        let mut record = recovery::RecoveryRecord {
            operation_id,
            phase: recovery::RecoveryPhase::StagePrepared,
            destination_parent: vec![
                recovery::RecoveryName::new_exact("destination").expect("outer parent name"),
                recovery::RecoveryName::new_exact("nested").expect("nested parent name"),
            ],
            destination_leaf: target,
            old: None,
            new: None,
        };
        let registration = journal.reserve(&record).expect("reserve recovery slot");
        journal
            .create_reserved(&first.authority.lease, registration, record.clone())
            .expect("persist prepared stage");
        let stage_path = destination.join(stage_name.as_str());
        std::fs::write(&stage_path, payload).expect("write recovery fixture stage");
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&stage_path)
            .expect("open recovery fixture stage")
            .sync_all()
            .expect("sync recovery fixture stage");
        record.phase = recovery::RecoveryPhase::StageSealed;
        record.new = Some(recovery::RecoveryFileProof {
            size: payload.len() as u64,
            sha256: Sha256::digest(payload).into(),
        });
        journal
            .advance(&first.authority.lease, registration, record)
            .expect("persist sealed stage");
        assert!(matches!(first.revoke(), RootRevokeOutcome::Revoked));

        let destination_for_hook = destination.clone();
        let displaced_for_hook = displaced.clone();
        #[cfg(unix)]
        recovery_runtime::set_replay_parent_validation_hook(move || {
            std::fs::rename(&destination_for_hook, &displaced_for_hook)
                .expect("displace replay parent");
            std::fs::create_dir(&destination_for_hook).expect("replace replay parent");
        });
        #[cfg(windows)]
        recovery_runtime::set_replay_parent_validation_hook(move || {
            std::fs::rename(&destination_for_hook, &displaced_for_hook)
                .expect_err("retained replay parent must prevent displacement");
            assert!(destination_for_hook.is_dir());
            assert!(!displaced_for_hook.exists());
        });

        #[cfg(unix)]
        {
            let obligation = match RootSession::acquire(temporary.path()) {
                RootSessionAcquireOutcome::AppliedUnverified(obligation)
                    if matches!(obligation.error(), RootSessionError::Recovery(_)) =>
                {
                    obligation
                }
                outcome => panic!("replay parent swap did not fail closed: {outcome:?}"),
            };
            assert!(!destination.join("replayed.bin").exists());
            assert!(displaced.join(stage_name.as_str()).is_file());

            std::fs::remove_dir(&destination).expect("remove replacement parent");
            std::fs::rename(&displaced, &destination).expect("restore exact replay parent");
            let replayed = match obligation.reconcile() {
                RootSessionAcquireOutcome::Acquired(session) => session,
                outcome => panic!("restored replay parent did not reconcile: {outcome:?}"),
            };
            assert_eq!(
                std::fs::read(destination.join("replayed.bin")).expect("replayed payload"),
                payload
            );
            assert!(!stage_path.exists());
            assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
        }

        #[cfg(windows)]
        {
            let replayed = acquire_test_root(temporary.path());
            assert_eq!(
                std::fs::read(destination.join("replayed.bin")).expect("replayed payload"),
                payload
            );
            assert!(!stage_path.exists());
            assert!(matches!(replayed.revoke(), RootRevokeOutcome::Revoked));
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_lease_rejects_links_and_non_files_without_mutating_them() {
        use std::os::unix::fs::symlink;

        for fixture in ["symlink", "hardlink", "directory"] {
            let temporary = tempfile::tempdir().expect("temporary root");
            let lease = temporary.path().join(ROOT_LEASE_NAME);
            match fixture {
                "symlink" => {
                    std::fs::write(temporary.path().join("target"), b"target")
                        .expect("symlink target");
                    symlink("target", &lease).expect("lease symlink");
                }
                "hardlink" => {
                    let target = temporary.path().join("target");
                    std::fs::write(&target, b"target").expect("hardlink target");
                    std::fs::hard_link(target, &lease).expect("lease hardlink");
                }
                "directory" => std::fs::create_dir(&lease).expect("lease directory"),
                _ => unreachable!("closed fixture set"),
            }
            assert!(matches!(
                RootSession::acquire(temporary.path()),
                RootSessionAcquireOutcome::NoEffect(RootSessionError::Lease(_))
            ));
            assert!(std::fs::symlink_metadata(&lease).is_ok());
        }
    }

    #[cfg(unix)]
    #[test]
    fn root_clear_refuses_a_replaced_lease_binding_before_deleting_children() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let owned = temporary.path().join("owned.bin");
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        let displaced = temporary.path().join("displaced-lease");
        std::fs::write(&owned, b"owned").expect("owned file");
        let session = acquire_test_root(temporary.path());
        let reset = match session.begin_reset() {
            ResetStartOutcome::Ready(authority) => authority,
            outcome => panic!("reset did not become ready: {outcome:?}"),
        };
        std::fs::rename(&lease, &displaced).expect("displace retained lease");
        std::fs::write(&lease, b"replacement").expect("replacement lease");

        let failure = match reset.clear_root() {
            RootClearOutcome::Failed(failure) => failure,
            RootClearOutcome::Cleared(receipt) => {
                receipt.release().expect("release unexpected clear receipt");
                panic!("clear accepted a replaced lease binding");
            }
        };
        assert!(owned.is_file(), "clear mutated children before lease proof");
        std::fs::remove_file(&lease).expect("remove replacement lease");
        std::fs::rename(&displaced, &lease).expect("restore retained lease");
        let receipt = match failure.retry() {
            RootClearOutcome::Cleared(receipt) => receipt,
            RootClearOutcome::Failed(failure) => {
                let error = failure.error().to_string();
                failure
                    .acknowledge_preserved()
                    .expect("release failed clear authority");
                panic!("clear did not recover after lease restoration: {error}")
            }
        };
        assert!(!owned.exists());
        receipt.release().expect("release clear receipt");
    }

    #[cfg(windows)]
    #[test]
    fn windows_exclusive_lease_prevents_substitution_before_root_clear() {
        let temporary = tempfile::tempdir().expect("temporary root");
        let owned = temporary.path().join("owned.bin");
        let lease = temporary.path().join(ROOT_LEASE_NAME);
        let displaced = temporary.path().join("displaced-lease");
        std::fs::write(&owned, b"owned").expect("owned file");
        let session = acquire_test_root(temporary.path());

        std::fs::rename(&lease, &displaced)
            .expect_err("exclusive lease must prevent pre-clear displacement");
        File::create(&lease).expect_err("exclusive lease must prevent pre-clear substitution");
        assert!(
            owned.is_file(),
            "denied substitution must not mutate children"
        );

        let reset = match session.begin_reset() {
            ResetStartOutcome::Ready(authority) => authority,
            outcome => panic!("reset did not become ready: {outcome:?}"),
        };
        let receipt = match reset.clear_root() {
            RootClearOutcome::Cleared(receipt) => receipt,
            RootClearOutcome::Failed(failure) => {
                panic!("ordinary root clear failed: {}", failure.error())
            }
        };
        assert!(!owned.exists());
        receipt.release().expect("release clear receipt");
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
            root.entries(1)
                .expect_err("revoked capability must refuse")
                .kind(),
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
        assert!(
            first_keys
                .iter()
                .any(|key| leaf_name_equivalence_keys(same).contains(key))
        );
        assert!(
            !first_keys
                .iter()
                .any(|key| leaf_name_equivalence_keys(different).contains(key))
        );
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
        let directory_error =
            match directory.park_as(LeafName::new("WORLD").expect("directory alias")) {
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
        let file_error =
            match root.park_file_as(request, LeafName::new("STATE.BIN").expect("file alias")) {
                FileParkOutcome::NoEffect { error, request } => {
                    drop(request);
                    error
                }
                FileParkOutcome::Parked(_) => panic!("same-binding file park applied"),
                FileParkOutcome::Preserved { .. } => {
                    panic!("same-binding file park preserved changed content")
                }
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

        parked.validate_current().expect("validate preserved file");
        {
            let state = session
                .authority
                .operations
                .lock()
                .expect("operation state");
            assert_eq!(state.outstanding_effects, 1);
            assert_eq!(state.file_parks.len(), 1);
            assert!(
                state
                    .file_parks
                    .values()
                    .all(|record| record.cleanup.is_some())
            );
        }

        parked
            .acknowledge_preserved()
            .expect("acknowledge preserved file");

        assert!(!temporary.path().join("record.bin").exists());
        assert_eq!(
            std::fs::read(temporary.path().join("record.preserved")).expect("read preserved file"),
            b"payload",
        );
        let state = session
            .authority
            .operations
            .lock()
            .expect("operation state");
        assert_eq!(state.outstanding_effects, 0);
        assert!(state.file_parks.is_empty());
        drop(state);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[test]
    fn preserved_file_acknowledgement_restores_mismatched_park_authority() {
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
        assert_eq!(state.file_parks.len(), 1);
        assert!(
            state
                .file_parks
                .values()
                .all(|record| record.cleanup.is_some())
        );
        drop(state);

        discard_test_park_registration(parked);
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }

    #[cfg(unix)]
    #[test]
    fn preserved_file_acknowledgement_returns_mutated_park_authority() {
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
        assert_eq!(state.file_parks.len(), 1);
        assert!(
            state
                .file_parks
                .values()
                .all(|record| record.cleanup.is_some())
        );
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
        let failure = failure
            .retry()
            .expect_err("foreign revision remains foreign");
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
    fn stable_park_header_reserves_names_during_file_record_checkout() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::write(temporary.path().join("file.parked"), b"payload").expect("parked file");
        std::fs::create_dir(temporary.path().join("directory.parked")).expect("parked directory");
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
            .open_directory(&LeafName::new("directory.parked").expect("directory park leaf"))
            .expect("parked directory capability");
        let directory_revision = directory.revision().expect("directory revision");
        let conflicting_original = LeafName::new("FILE.ORIGINAL").expect("aliasing original leaf");
        let conflicting_park = LeafName::new("directory.parked").expect("directory park leaf");

        let authority = parked_file.authority().expect("park authority");
        let operation = authority
            .enter_file_park(&parked_file.token)
            .expect("file park operation");
        let guard = authority
            .take_file_park(&operation, &parked_file.token)
            .expect("checked-out file park");
        let mut unrelated = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("unrelated-source").expect("unrelated source"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("unrelated-destination").expect("unrelated destination"),
            },
            None,
            None,
            None,
        )
        .expect("checked-out park must not block an unrelated sibling");
        unrelated
            .settle(&operation)
            .expect("settle unrelated move reservation");
        let checked_out_conflict = authority
            .ensure_park_available(
                &operation,
                &root,
                &conflicting_original,
                &conflicting_park,
                Some(directory.inner.identity.physical),
            )
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

    #[test]
    fn stable_headers_reserve_checked_out_creates_and_directory_parks() {
        let temporary = tempfile::tempdir().expect("temporary root");
        std::fs::create_dir(temporary.path().join("folder")).expect("parked directory");
        let session = acquire_test_root(temporary.path());
        let root = session.root().expect("root capability");
        let authority = root.authority().expect("root authority");
        let operation = authority.enter().expect("create reservation operation");

        let stage_name = LeafName::new("stage-create").expect("stage create leaf");
        let mut stage_create = authority
            .reserve_stage_create(&operation, &root, &stage_name, None)
            .expect("stage create reservation");
        let stage_guard = authority
            .take_stage_create(&operation, &stage_create)
            .expect("checked-out stage create");
        let stage_conflict = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: stage_name,
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("stage-conflict-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect_err("checked-out stage create must retain its leaf");
        assert_eq!(stage_conflict.kind(), io::ErrorKind::WouldBlock);
        let mut unrelated = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("stage-unrelated-source").expect("source"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("stage-unrelated-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect("checked-out stage create must not block a sibling");
        unrelated.settle(&operation).expect("settle sibling move");
        stage_guard
            .disarm(&mut stage_create, &operation)
            .expect("checked-out stage create settles");

        let directory_name = LeafName::new("directory-create").expect("directory create leaf");
        let mut directory_create = authority
            .reserve_directory_create(&operation, &root, &directory_name)
            .expect("directory create reservation");
        let directory_guard = authority
            .take_directory_create(&operation, &directory_create)
            .expect("checked-out directory create");
        let directory_conflict = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: directory_name,
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("directory-conflict-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect_err("checked-out directory create must retain its leaf");
        assert_eq!(directory_conflict.kind(), io::ErrorKind::WouldBlock);
        let unrelated = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("directory-unrelated-source").expect("source"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("directory-unrelated-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect_err("identity-unknown directory create must fail closed globally");
        assert_eq!(unrelated.kind(), io::ErrorKind::WouldBlock);
        directory_guard.disarm(&mut directory_create, &operation);
        let mut unrelated = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("directory-unrelated-source").expect("source"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("directory-unrelated-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect("settled directory create releases unrelated siblings");
        unrelated.settle(&operation).expect("settle sibling move");
        drop(operation);

        let parked = park_test_directory(&root, "folder", "folder.park");
        let operation = authority
            .enter_directory_park(&parked.token)
            .expect("directory park operation");
        let guard = authority
            .take_directory_park(&operation, &parked.token)
            .expect("checked-out directory park");
        let park_conflict = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("folder").expect("original leaf"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("park-conflict-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect_err("checked-out directory park must retain its original leaf");
        assert_eq!(park_conflict.kind(), io::ErrorKind::WouldBlock);
        let mut unrelated = MoveEffectToken::reserve(
            &authority,
            &operation,
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("park-unrelated-source").expect("source"),
            },
            NamespaceLeaf {
                parent: root.clone(),
                name: LeafName::new("park-unrelated-destination").expect("destination"),
            },
            None,
            None,
            None,
        )
        .expect("checked-out directory park must not block a sibling");
        unrelated.settle(&operation).expect("settle sibling move");
        drop(guard);
        drop(operation);
        let restored = match parked.restore() {
            DirectoryRestoreOutcome::Restored(directory) => directory,
            DirectoryRestoreOutcome::NoEffect { error, .. } => {
                panic!("directory restoration had no effect: {error}")
            }
            DirectoryRestoreOutcome::AppliedUnverified(obligation) => {
                panic!(
                    "directory restoration was indeterminate: {}",
                    obligation.error()
                )
            }
        };

        drop((restored, authority, root));
        assert!(matches!(session.revoke(), RootRevokeOutcome::Revoked));
    }
}
