//! Identity-bound access to exact regular files below held no-follow directories.

use std::ffi::{OsStr, OsString};
use std::io;
use std::io::Read as _;
use std::path::Path;
use std::sync::Arc;

use axial_config::AppRootSession;
use axial_fs::{
    Directory, DirectoryListingState, DirectoryRevision, ExpectedFileContent, FileParkObligation,
    FileParkOutcome, FileParkPreservationError, FileParkResolution, FileRevision, LeafName,
    ParkedFile,
};
use sha2::{Digest as _, Sha256};
use sha2::Sha512;

const RESTART_IDENTITY_DOMAIN: &[u8] = b"axial.persisted-state-restart-record-identity.v3\0";
const MAX_DIRECTORY_ENTRIES: usize = 100_000;
#[cfg(any(unix, windows))]
const MAX_DIRECT_LEAF_UNITS: usize = 255;

pub(crate) struct AnchoredRecordIdentity {
    directory: Directory,
    leaf: LeafName,
    revision: FileRevision,
    quarantine_sha256: Option<[u8; 32]>,
    root_session: Arc<AppRootSession>,
}

#[must_use = "parked-file receipt must be acknowledged or retained"]
pub(crate) struct AnchoredRecordQuarantineReceipt {
    parked: ParkedFile,
    _root_session: Arc<AppRootSession>,
}

#[must_use = "unsettled quarantine preservation retains parked-file authority"]
pub(crate) enum AnchoredRecordQuarantinePreservationError {
    Acknowledgement {
        error: FileParkPreservationError,
        _root_session: Arc<AppRootSession>,
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

enum RegisteredArtifactQuarantineError {
    Refused(io::Error),
    AppliedUnverified(io::Error),
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

#[derive(Clone, Copy)]
pub(crate) enum AnchoredRecordRestartContext {
    PerformanceOperation,
    BenchmarkSuiteDriver,
}

#[derive(Clone)]
pub(crate) struct AnchoredRecordDirectory {
    directory: Directory,
    root_session: Arc<AppRootSession>,
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

struct AnchoredLeaf(platform::Leaf);
struct AnchoredRegularFile(platform::RegularFile);
struct AnchoredTemp(platform::Temp);

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
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test_directory(path: &Path) -> io::Result<Self> {
        let paths = axial_config::AppPaths::from_root(path).map_err(io::Error::other)?;
        let root_session = Arc::new(paths.open_root_session()?);
        let directory = root_session.root_directory()?;
        Ok(Self::from_directory(root_session, directory))
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
            directory: self.directory.clone(),
            leaf,
            revision,
            quarantine_sha256: Some(sha256),
            root_session: Arc::clone(&self.root_session),
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

    pub(crate) fn read_for_mutation(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordObservation> {
        self.read_inner(name, max_bytes)
    }

    fn read_inner(&self, name: &OsStr, max_bytes: u64) -> io::Result<AnchoredRecordObservation> {
        let leaf = capability_leaf(name)?;
        let file = self.directory.open_file(&leaf)?;
        let revision = file.revision()?;
        if revision.size() > max_bytes {
            let identity = AnchoredRecordIdentity {
                directory: self.directory.clone(),
                leaf,
                revision,
                quarantine_sha256: None,
                root_session: Arc::clone(&self.root_session),
            };
            identity.revalidate()?;
            return Ok(AnchoredRecordObservation::Oversized { identity });
        }
        let bytes = file.read_bounded(max_bytes)?;
        file.validate_revision(&revision)?;
        let sha256 = Sha256::digest(&bytes).into();
        let identity = AnchoredRecordIdentity {
            directory: self.directory.clone(),
            leaf,
            revision,
            quarantine_sha256: Some(sha256),
            root_session: Arc::clone(&self.root_session),
        };
        identity.revalidate()?;
        Ok(AnchoredRecordObservation::Bytes { bytes, identity })
    }
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
        self.directory
            .open_file(&self.leaf)?
            .validate_revision(&self.revision)
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
        let file = self
            .directory
            .open_file(&self.leaf)
            .map_err(AnchoredRecordQuarantineError::Refused)?;
        file.validate_revision(&self.revision)
            .map_err(AnchoredRecordQuarantineError::Refused)?;
        let request = file.park_request(ExpectedFileContent::new(self.revision, sha256));
        settle_capability_park(
            self.directory.park_file_as(request, destination),
            self.root_session,
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
        let file = self.directory.open_file(&self.leaf)?;
        file.validate_revision(&self.revision)?;
        let request = file.park_request(ExpectedFileContent::new(self.revision, sha256));
        self.directory
            .admit_existing_file_park(&original, request)
            .map(|parked| AnchoredRecordQuarantineReceipt {
                parked,
                _root_session: self.root_session,
            })
    }

}

impl AnchoredRecordQuarantineReceipt {
    pub(crate) fn is_current(&self) -> bool {
        self.parked.validate_current().is_ok()
    }

    pub(crate) fn acknowledge_preserved(
        self,
    ) -> Result<(), AnchoredRecordQuarantinePreservationError> {
        self.parked.acknowledge_preserved().map_err(|error| {
            AnchoredRecordQuarantinePreservationError::Acknowledgement {
                error,
                _root_session: self._root_session,
            }
        })
    }

    pub(crate) fn acknowledge_applied_unverified(
        self,
    ) -> Option<AnchoredRecordQuarantinePreservationError> {
        self.parked
            .acknowledge_preserved()
            .err()
            .map(|error| AnchoredRecordQuarantinePreservationError::Acknowledgement {
                error,
                _root_session: self._root_session,
            })
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
            Self::IndeterminatePark { .. } => formatter
                .write_str("anchored record quarantine preservation remains indeterminate"),
        }
    }
}

impl std::error::Error for AnchoredRecordQuarantinePreservationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Acknowledgement { error, .. } => Some(error.error()),
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

impl RegisteredArtifactQuarantineError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::Refused(error) | Self::AppliedUnverified(error) => error,
        }
    }
}

fn settle_capability_park(
    outcome: FileParkOutcome,
    root_session: Arc<AppRootSession>,
) -> Result<AnchoredRecordQuarantineReceipt, AnchoredRecordQuarantineError> {
    match outcome {
        FileParkOutcome::Parked(parked) => Ok(AnchoredRecordQuarantineReceipt {
            parked,
            _root_session: root_session,
        }),
        FileParkOutcome::NoEffect { error, .. } => {
            Err(AnchoredRecordQuarantineError::Refused(error))
        }
        FileParkOutcome::AppliedUnverified(obligation) => {
            let error = io::Error::new(obligation.error().kind(), obligation.error().to_string());
            match obligation.reconcile() {
                FileParkResolution::Parked(parked) => Ok(AnchoredRecordQuarantineReceipt {
                    parked,
                    _root_session: root_session,
                }),
                FileParkResolution::NoEffect(_) => {
                    Err(AnchoredRecordQuarantineError::Refused(error))
                }
                FileParkResolution::Indeterminate(obligation) => {
                    Err(AnchoredRecordQuarantineError::AppliedUnverified {
                        obligation,
                        _root_session: root_session,
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

impl AnchoredLeaf {
    fn open(root: &Path, relative: &Path) -> io::Result<Self> {
        platform::Leaf::open(root, relative).map(Self)
    }

    fn revalidate(&self) -> io::Result<()> {
        self.0.revalidate()
    }

    fn target_is_missing(&self) -> io::Result<bool> {
        self.0.target_is_missing()
    }

    fn quarantine_existing(&self) -> io::Result<()> {
        let file = self
            .open_regular_with_intent(true)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "anchored record is missing"))?;
        let destination = OsString::from(format!(
            ".axial-quarantine-{}",
            uuid::Uuid::new_v4().simple()
        ));
        self.rename_exact(file, destination)
            .map(|_| ())
            .map_err(RegisteredArtifactQuarantineError::into_io_error)
    }

    fn rename_exact(
        &self,
        file: AnchoredRegularFile,
        destination: OsString,
    ) -> Result<platform::ExactRenameReceipt, RegisteredArtifactQuarantineError> {
        self.0.rename_exact(file.0, destination)
    }

    fn create_temp(&self) -> io::Result<AnchoredTemp> {
        self.0.create_temp().map(AnchoredTemp)
    }

    fn remove_temp(&self, temp: AnchoredTemp) {
        self.0.remove_temp(temp.0);
    }

    fn promote_temp(&self, temp: &AnchoredTemp) -> io::Result<()> {
        self.0.promote_temp(&temp.0)
    }

    fn open_regular(&self) -> io::Result<Option<AnchoredRegularFile>> {
        self.open_regular_with_intent(false)
    }

    fn open_regular_with_intent(
        &self,
        mutation_compatible: bool,
    ) -> io::Result<Option<AnchoredRegularFile>> {
        self.0
            .open_regular(mutation_compatible)
            .map(|file| file.map(AnchoredRegularFile))
    }
}

impl AnchoredRegularFile {
    fn verify_sha1(self, expected_sha1: &str, expected_size: u64) -> Option<Self> {
        self.0.verify_sha1(expected_sha1, expected_size).map(Self)
    }

    fn revalidate(&self) -> bool {
        self.0.revalidate()
    }
}

impl AnchoredTemp {
    fn take_writer(&mut self) -> Option<std::fs::File> {
        self.0.take_writer()
    }
}

#[cfg(unix)]
mod platform {
    use rustix::fd::OwnedFd;
    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
    use rustix::fs::RenameFlags;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use sha1::{Digest as _, Sha1};
    use std::ffi::{OsStr, OsString};
    use std::io::{self, Read as _, Seek as _, SeekFrom};
    use std::os::unix::ffi::OsStrExt as _;
    use std::path::{Component, Path, PathBuf};
    use std::sync::Arc;

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct CanonicalFileIdentity {
        device: u64,
        inode: u64,
    }

    #[derive(Clone, Copy, Eq, PartialEq)]
    struct CanonicalRegularFileMetadata {
        identity: CanonicalFileIdentity,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: u64,
        changed_seconds: i64,
        changed_nanoseconds: u64,
    }

    #[derive(Clone)]
    struct HeldDirectory {
        handle: Arc<OwnedFd>,
        parent: Option<Arc<OwnedFd>>,
        name: Option<OsString>,
        identity: CanonicalFileIdentity,
    }

    #[derive(Clone)]
    pub(super) struct Leaf {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<OwnedFd>,
        leaf: OsString,
    }

    pub(super) struct RegularFile {
        parent: Arc<OwnedFd>,
        leaf: OsString,
        file: std::fs::File,
        metadata: CanonicalRegularFileMetadata,
    }

    pub(super) struct ExactRenameReceipt {
        leaf: Leaf,
        destination: OsString,
        file: RegularFile,
    }

    pub(super) struct Temp {
        name: OsString,
        writer: Option<std::fs::File>,
        control: std::fs::File,
        identity: CanonicalFileIdentity,
    }

    impl Temp {
        pub(super) fn take_writer(&mut self) -> Option<std::fs::File> {
            self.writer.take()
        }
    }


    impl Leaf {
        pub(super) fn open(root: &Path, relative: &Path) -> io::Result<Self> {
            let root_path = PathBuf::from("/");
            let mut directories = open_absolute_directory_chain(root)?;
            let mut parent = directories
                .last()
                .expect("absolute directory chain has anchor")
                .handle
                .clone();
            let mut components = relative.components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "anchored record path escaped its root",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                let child = rustix::fs::openat(
                    parent.as_ref(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?;
                let stat = rustix::fs::fstat(&child).map_err(io::Error::from)?;
                if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "anchored record ancestor is not a directory",
                    ));
                }
                let child = Arc::new(child);
                directories.push(HeldDirectory {
                    handle: child.clone(),
                    parent: Some(parent),
                    name: Some(name.to_os_string()),
                    identity: canonical_file_identity(&stat)?,
                });
                parent = child;
            }
            let leaf = leaf.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is empty",
                )
            })?;
            let anchored = Self {
                root_path,
                directories,
                parent,
                leaf,
            };
            anchored.revalidate()?;
            Ok(anchored)
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            revalidate_directory_chain(&self.root_path, &self.directories)
        }


        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            match rustix::fs::statat(self.parent.as_ref(), &self.leaf, AtFlags::SYMLINK_NOFOLLOW) {
                Ok(_) => Ok(false),
                Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(io::Error::from(error)),
            }
        }

        pub(super) fn rename_exact(
            &self,
            mut file: RegularFile,
            destination: OsString,
        ) -> Result<ExactRenameReceipt, super::RegisteredArtifactQuarantineError> {
            use super::RegisteredArtifactQuarantineError::{AppliedUnverified, Refused};

            require_exact_noreplace_rename().map_err(Refused)?;
            require_direct_leaf(&destination).map_err(Refused)?;
            if destination == self.leaf
                || !Arc::ptr_eq(&self.parent, &file.parent)
                || file.leaf != self.leaf
            {
                return Err(Refused(identity_changed(
                    "anchored record rename authority does not match its source",
                )));
            }
            self.revalidate().map_err(Refused)?;
            if !file.revalidate() {
                return Err(Refused(identity_changed(
                    "anchored record identity changed before rename",
                )));
            }
            match rustix::fs::statat(
                self.parent.as_ref(),
                &destination,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(_) => {
                    return Err(Refused(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "anchored record rename destination exists",
                    )));
                }
                Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(Refused(io::Error::from(error))),
            }
            self.revalidate().map_err(Refused)?;
            if !file.revalidate() {
                return Err(Refused(identity_changed(
                    "anchored record identity changed before rename",
                )));
            }
            match rustix::fs::statat(
                self.parent.as_ref(),
                &destination,
                AtFlags::SYMLINK_NOFOLLOW,
            ) {
                Ok(_) => {
                    return Err(Refused(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "anchored record rename destination exists",
                    )));
                }
                Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(Refused(io::Error::from(error))),
            }
            renameat_noreplace(
                self.parent.as_ref(),
                &self.leaf,
                self.parent.as_ref(),
                &destination,
            )
            .map_err(Refused)?;
            file.reseal_after_rename().map_err(AppliedUnverified)?;
            let receipt = ExactRenameReceipt {
                leaf: self.clone(),
                destination,
                file,
            };
            receipt.revalidate().map_err(AppliedUnverified)?;
            rustix::fs::fsync(self.parent.as_ref())
                .map_err(|error| AppliedUnverified(io::Error::from(error)))?;
            receipt.revalidate().map_err(AppliedUnverified)?;
            Ok(receipt)
        }

        pub(super) fn create_temp(&self) -> io::Result<Temp> {
            self.revalidate()?;
            let name = OsString::from(format!(
                ".axial-repair-{}.tmp",
                uuid::Uuid::new_v4().simple()
            ));
            let handle = rustix::fs::openat(
                self.parent.as_ref(),
                &name,
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::from_bits_truncate(0o600),
            )
            .map_err(io::Error::from)?;
            let writer = std::fs::File::from(handle);
            let control = writer.try_clone()?;
            let stat = rustix::fs::fstat(&control).map_err(io::Error::from)?;
            Ok(Temp {
                name,
                writer: Some(writer),
                control,
                identity: canonical_file_identity(&stat)?,
            })
        }

        pub(super) fn remove_temp(&self, temp: Temp) {
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW);
            if current
                .is_ok_and(|current| canonical_file_identity(&current).ok() == Some(temp.identity))
            {
                let _ = rustix::fs::unlinkat(self.parent.as_ref(), &temp.name, AtFlags::empty());
            }
            let _ = rustix::fs::fsync(self.parent.as_ref());
        }

        pub(super) fn promote_temp(&self, temp: &Temp) -> io::Result<()> {
            require_exact_noreplace_rename()?;
            let held = rustix::fs::fstat(&temp.control).map_err(io::Error::from)?;
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            if canonical_file_identity(&held)? != temp.identity
                || canonical_file_identity(&current)? != temp.identity
                || FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
            {
                return Err(identity_changed("anchored record temp changed"));
            }
            renameat_noreplace(
                self.parent.as_ref(),
                &temp.name,
                self.parent.as_ref(),
                &self.leaf,
            )?;
            rustix::fs::fsync(self.parent.as_ref()).map_err(io::Error::from)
        }

        pub(super) fn open_regular(
            &self,
            _mutation_compatible: bool,
        ) -> io::Result<Option<RegularFile>> {
            self.revalidate()?;
            let handle = match rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(handle) => handle,
                Err(error) if io::Error::from(error).kind() == io::ErrorKind::NotFound => {
                    return Ok(None);
                }
                Err(error) => return Err(io::Error::from(error)),
            };
            let stat = rustix::fs::fstat(&handle).map_err(io::Error::from)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile || stat.st_nlink != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is not an exact regular file",
                ));
            }
            let metadata = canonical_regular_file_metadata(&stat)?;
            let file = RegularFile {
                parent: self.parent.clone(),
                leaf: self.leaf.clone(),
                file: std::fs::File::from(handle),
                metadata,
            };
            if !file.revalidate() {
                return Err(identity_changed(
                    "anchored record identity changed during admission",
                ));
            }
            Ok(Some(file))
        }
    }

    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
    fn require_exact_noreplace_rename() -> io::Result<()> {
        Ok(())
    }

    #[cfg(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    ))]
    fn renameat_noreplace(
        source_parent: &OwnedFd,
        source: &OsStr,
        destination_parent: &OwnedFd,
        destination: &OsStr,
    ) -> io::Result<()> {
        rustix::fs::renameat_with(
            source_parent,
            source,
            destination_parent,
            destination,
            RenameFlags::NOREPLACE,
        )
        .map_err(io::Error::from)
    }

    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    )))]
    fn require_exact_noreplace_rename() -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "exact no-replace rename is unavailable on this Unix target",
        ))
    }

    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    )))]
    fn renameat_noreplace(
        _source_parent: &OwnedFd,
        _source: &OsStr,
        _destination_parent: &OwnedFd,
        _destination: &OsStr,
    ) -> io::Result<()> {
        require_exact_noreplace_rename()
    }

    impl RegularFile {

        pub(super) fn verify_sha1(
            mut self,
            expected_sha1: &str,
            expected_size: u64,
        ) -> Option<Self> {
            if self.metadata.size != expected_size {
                return None;
            }
            self.file.seek(SeekFrom::Start(0)).ok()?;
            let mut hasher = Sha1::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = self.file.read(&mut buffer).ok()?;
                if count == 0 {
                    break;
                }
                observed = observed.checked_add(count as u64)?;
                if observed > expected_size {
                    return None;
                }
                hasher.update(&buffer[..count]);
            }
            (observed == expected_size
                && format!("{:x}", hasher.finalize()) == expected_sha1
                && self.revalidate())
            .then_some(self)
        }


        pub(super) fn revalidate(&self) -> bool {
            let Ok(held) = rustix::fs::fstat(&self.file) else {
                return false;
            };
            if !self.matches(held) {
                return false;
            }
            let current = match rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            ) {
                Ok(current) => current,
                Err(_) => return false,
            };
            rustix::fs::fstat(&current).is_ok_and(|current| self.matches(current))
        }

        fn reseal_after_rename(&mut self) -> io::Result<()> {
            let held = rustix::fs::fstat(&self.file).map_err(io::Error::from)?;
            let metadata = canonical_regular_file_metadata(&held)?;
            if metadata.identity != self.metadata.identity
                || metadata.size != self.metadata.size
                || metadata.modified_seconds != self.metadata.modified_seconds
                || metadata.modified_nanoseconds != self.metadata.modified_nanoseconds
            {
                return Err(identity_changed(
                    "anchored record changed while it was being renamed",
                ));
            }
            self.metadata = metadata;
            Ok(())
        }

        fn held_is_current(&self) -> bool {
            rustix::fs::fstat(&self.file).is_ok_and(|held| self.matches(held))
        }

        fn matches(&self, stat: rustix::fs::Stat) -> bool {
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_nlink == 1
                && canonical_regular_file_metadata(&stat).ok() == Some(self.metadata)
        }
    }

    impl ExactRenameReceipt {
        pub(super) fn revalidate(&self) -> io::Result<()> {
            self.leaf.revalidate()?;
            if !self.file.held_is_current() || !self.leaf.target_is_missing()? {
                return Err(identity_changed(
                    "anchored record rename receipt is no longer current",
                ));
            }
            let destination = rustix::fs::statat(
                self.leaf.parent.as_ref(),
                &self.destination,
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io::Error::from)?;
            if !self.file.matches(destination) {
                return Err(identity_changed(
                    "anchored record rename destination changed identity",
                ));
            }
            self.leaf.revalidate()
        }
    }

    fn canonical_file_identity(stat: &rustix::fs::Stat) -> io::Result<CanonicalFileIdentity> {
        Ok(CanonicalFileIdentity {
            device: canonical_unsigned(stat.st_dev, "device")?,
            inode: canonical_unsigned(stat.st_ino, "inode")?,
        })
    }

    fn canonical_regular_file_metadata(
        stat: &rustix::fs::Stat,
    ) -> io::Result<CanonicalRegularFileMetadata> {
        Ok(CanonicalRegularFileMetadata {
            identity: canonical_file_identity(stat)?,
            size: canonical_unsigned(stat.st_size, "size")?,
            modified_seconds: canonical_signed(stat.st_mtime, "modified seconds")?,
            modified_nanoseconds: canonical_nanoseconds(
                stat.st_mtime_nsec,
                "modified nanoseconds",
            )?,
            changed_seconds: canonical_signed(stat.st_ctime, "changed seconds")?,
            changed_nanoseconds: canonical_nanoseconds(stat.st_ctime_nsec, "changed nanoseconds")?,
        })
    }

    fn canonical_unsigned<T>(value: T, field: &'static str) -> io::Result<u64>
    where
        T: TryInto<u64>,
    {
        value.try_into().map_err(|_| invalid_stat_field(field))
    }

    fn canonical_signed<T>(value: T, field: &'static str) -> io::Result<i64>
    where
        T: TryInto<i64>,
    {
        value.try_into().map_err(|_| invalid_stat_field(field))
    }

    fn canonical_nanoseconds<T>(value: T, field: &'static str) -> io::Result<u64>
    where
        T: TryInto<u64>,
    {
        let value = canonical_unsigned(value, field)?;
        if value < 1_000_000_000 {
            Ok(value)
        } else {
            Err(invalid_stat_field(field))
        }
    }

    fn invalid_stat_field(field: &'static str) -> io::Error {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("anchored record {field} is invalid"),
        )
    }

    fn open_absolute_directory_chain(path: &Path) -> io::Result<Vec<HeldDirectory>> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record root is not absolute",
            ));
        }
        let root = rustix::fs::open(
            "/",
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let stat = rustix::fs::fstat(&root).map_err(io::Error::from)?;
        let root = Arc::new(root);
        let mut directories = vec![HeldDirectory {
            handle: root.clone(),
            parent: None,
            name: None,
            identity: canonical_file_identity(&stat)?,
        }];
        let mut parent = root;
        for component in path.components() {
            let name = match component {
                Component::RootDir => continue,
                Component::Normal(name) => name,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "anchored record root is not normalized",
                    ));
                }
            };
            let child = rustix::fs::openat(
                parent.as_ref(),
                name,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let stat = rustix::fs::fstat(&child).map_err(io::Error::from)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record root ancestor is not a directory",
                ));
            }
            let child = Arc::new(child);
            directories.push(HeldDirectory {
                handle: child.clone(),
                parent: Some(parent),
                name: Some(name.to_os_string()),
                identity: canonical_file_identity(&stat)?,
            });
            parent = child;
        }
        Ok(directories)
    }

    fn revalidate_directory_chain(
        root_path: &Path,
        directories: &[HeldDirectory],
    ) -> io::Result<()> {
        let root = rustix::fs::open(
            root_path,
            OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?;
        let root_stat = rustix::fs::fstat(&root).map_err(io::Error::from)?;
        let expected_root = &directories[0];
        if canonical_file_identity(&root_stat)? != expected_root.identity {
            return Err(identity_changed("anchored record root changed"));
        }
        let held_root =
            rustix::fs::fstat(expected_root.handle.as_ref()).map_err(io::Error::from)?;
        if canonical_file_identity(&held_root)? != expected_root.identity {
            return Err(identity_changed("anchored record held root changed"));
        }
        for directory in directories.iter().skip(1) {
            let stat = rustix::fs::statat(
                directory
                    .parent
                    .as_ref()
                    .expect("child has held parent")
                    .as_ref(),
                directory.name.as_ref().expect("child has name"),
                AtFlags::SYMLINK_NOFOLLOW,
            )
            .map_err(io::Error::from)?;
            if FileType::from_raw_mode(stat.st_mode) != FileType::Directory
                || canonical_file_identity(&stat)? != directory.identity
            {
                return Err(identity_changed("anchored record ancestor changed"));
            }
            let held = rustix::fs::fstat(directory.handle.as_ref()).map_err(io::Error::from)?;
            if canonical_file_identity(&held)? != directory.identity {
                return Err(identity_changed("anchored record held ancestor changed"));
            }
        }
        Ok(())
    }

    fn require_direct_leaf(name: &OsStr) -> io::Result<()> {
        if name.as_bytes().is_empty() || name.as_bytes().len() > super::MAX_DIRECT_LEAF_UNITS {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored record name exceeds the supported direct-leaf bound",
            ));
        }
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(component)) if component == name)
            || components.next().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record name is not a direct leaf",
            ));
        }
        Ok(())
    }

    fn identity_changed(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    #[cfg(test)]
    mod normalization_tests {
        use super::{
            canonical_nanoseconds, canonical_signed, canonical_unsigned, require_direct_leaf,
        };
        use std::ffi::OsStr;

        #[test]
        fn direct_leaf_bound_counts_unix_bytes() {
            let multi_byte_at_bound = "x".repeat(253) + "é";
            let multi_byte_over_bound = "x".repeat(254) + "é";
            assert!(require_direct_leaf(OsStr::new(&"x".repeat(255))).is_ok());
            assert!(require_direct_leaf(OsStr::new(&"x".repeat(256))).is_err());
            assert!(require_direct_leaf(OsStr::new(&multi_byte_at_bound)).is_ok());
            assert!(require_direct_leaf(OsStr::new(&multi_byte_over_bound)).is_err());
        }

        #[test]
        fn stat_fields_are_checked_before_entering_canonical_identity() {
            assert_eq!(canonical_unsigned(7_i32, "test").unwrap(), 7_u64);
            assert!(canonical_unsigned(-1_i32, "test").is_err());
            assert_eq!(
                canonical_signed(i32::MIN, "test").unwrap(),
                i64::from(i32::MIN)
            );
            assert!(canonical_signed(u64::MAX, "test").is_err());
            assert_eq!(
                canonical_nanoseconds(999_999_999_i64, "test").unwrap(),
                999_999_999_u64
            );
            assert!(canonical_nanoseconds(-1_i64, "test").is_err());
            assert!(canonical_nanoseconds(1_000_000_000_u64, "test").is_err());
        }
    }
}

#[cfg(windows)]
mod platform {
    use sha1::{Digest as _, Sha1};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, Read as _, Seek as _, SeekFrom};
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::fs::OpenOptionsExt as _;
    use std::os::windows::io::{AsRawHandle as _, FromRawHandle as _};
    use std::path::{Component, Path, PathBuf, Prefix};
    use std::ptr;
    use std::sync::Arc;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_CREATE, FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_RENAME_INFORMATION, FILE_SYNCHRONOUS_IO_NONALERT,
        FileRenameInformation, NtCreateFile, NtSetInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE, OBJ_CASE_INSENSITIVE,
        RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_INFO, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FILE_TRAVERSE, FileBasicInfo, FileDispositionInfo, FileIdInfo,
        FileStandardInfo, GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    #[derive(Clone)]
    struct HeldDirectory {
        handle: Arc<fs::File>,
        parent: Option<Arc<fs::File>>,
        name: Option<OsString>,
        volume: u64,
        id: [u8; 16],
    }

    #[derive(Clone)]
    pub(super) struct Leaf {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<fs::File>,
        leaf: OsString,
    }

    pub(super) struct RegularFile {
        parent: Arc<fs::File>,
        leaf: OsString,
        file: fs::File,
        volume: u64,
        id: [u8; 16],
        size: i64,
        modified: i64,
        changed: i64,
        share_mode: u32,
    }

    pub(super) struct ExactRenameReceipt {
        leaf: Leaf,
        mutation_parent: Arc<fs::File>,
        destination: OsString,
        file: RegularFile,
    }

    pub(super) struct Temp {
        writer: Option<fs::File>,
        control: fs::File,
        volume: u64,
        id: [u8; 16],
    }

    impl Temp {
        pub(super) fn take_writer(&mut self) -> Option<fs::File> {
            self.writer.take()
        }
    }


    impl Leaf {
        pub(super) fn open(root: &Path, relative: &Path) -> io::Result<Self> {
            let (root_path, mut directories) = open_absolute_directory_chain(root)?;
            let mut parent = directories
                .last()
                .expect("absolute directory chain has anchor")
                .handle
                .clone();
            let mut components = relative.components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "anchored record path escaped its root",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                let child = Arc::new(open_relative(
                    &parent,
                    name,
                    Some(true),
                    FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                )?);
                require_exact_directory(&child)?;
                let id = query::<FILE_ID_INFO>(&child, FileIdInfo)?;
                directories.push(HeldDirectory {
                    handle: child.clone(),
                    parent: Some(parent),
                    name: Some(name.to_os_string()),
                    volume: id.VolumeSerialNumber,
                    id: id.FileId.Identifier,
                });
                parent = child;
            }
            let leaf = leaf.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is empty",
                )
            })?;
            let anchored = Self {
                root_path,
                directories,
                parent,
                leaf,
            };
            anchored.revalidate()?;
            Ok(anchored)
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            revalidate_directory_chain(&self.root_path, &self.directories)
        }


        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            match open_relative(
                &self.parent,
                &self.leaf,
                None,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
            ) {
                Ok(_) => Ok(false),
                Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
                Err(error) => Err(error),
            }
        }

        pub(super) fn rename_exact(
            &self,
            mut file: RegularFile,
            destination: OsString,
        ) -> Result<ExactRenameReceipt, super::RegisteredArtifactQuarantineError> {
            use super::RegisteredArtifactQuarantineError::{AppliedUnverified, Refused};

            require_direct_leaf(&destination).map_err(Refused)?;
            if destination == self.leaf
                || !Arc::ptr_eq(&self.parent, &file.parent)
                || file.leaf != self.leaf
            {
                return Err(Refused(identity_changed(
                    "anchored record rename authority does not match its source",
                )));
            }
            let mutation_parent = self.open_mutation_parent().map_err(Refused)?;
            self.revalidate().map_err(Refused)?;
            if !file.revalidate() {
                return Err(Refused(identity_changed(
                    "anchored record identity changed before rename",
                )));
            }
            require_target_missing(&mutation_parent, &destination).map_err(Refused)?;
            self.revalidate().map_err(Refused)?;
            self.revalidate_mutation_parent(&mutation_parent)
                .map_err(Refused)?;
            if !file.revalidate() {
                return Err(Refused(identity_changed(
                    "anchored record identity changed before rename",
                )));
            }
            require_target_missing(&mutation_parent, &destination).map_err(Refused)?;
            rename_relative(&file.file, &mutation_parent, &destination).map_err(Refused)?;
            file.reseal_after_rename().map_err(AppliedUnverified)?;
            let receipt = ExactRenameReceipt {
                leaf: self.clone(),
                mutation_parent,
                destination,
                file,
            };
            receipt.revalidate().map_err(AppliedUnverified)?;
            receipt
                .mutation_parent
                .sync_all()
                .map_err(AppliedUnverified)?;
            receipt.revalidate().map_err(AppliedUnverified)?;
            Ok(receipt)
        }

        fn open_mutation_parent(&self) -> io::Result<Arc<fs::File>> {
            let expected = self
                .directories
                .last()
                .expect("absolute directory chain has anchor");
            let parent = Arc::new(match (&expected.parent, &expected.name) {
                (Some(parent), Some(name)) => open_relative(
                    parent,
                    name,
                    Some(true),
                    GENERIC_WRITE | FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                )?,
                (None, None) => open_root_exact_with_access(
                    &self.root_path,
                    GENERIC_WRITE | FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                )?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored directory chain is incoherent",
                    ));
                }
            });
            self.revalidate_mutation_parent(&parent)?;
            Ok(parent)
        }

        fn revalidate_mutation_parent(&self, parent: &fs::File) -> io::Result<()> {
            require_exact_directory(parent)?;
            let expected = self
                .directories
                .last()
                .expect("absolute directory chain has anchor");
            let id = query::<FILE_ID_INFO>(parent, FileIdInfo)?;
            if id.VolumeSerialNumber != expected.volume || id.FileId.Identifier != expected.id {
                return Err(identity_changed(
                    "anchored record mutation parent changed identity",
                ));
            }
            Ok(())
        }

        pub(super) fn create_temp(&self) -> io::Result<Temp> {
            self.revalidate()?;
            let name = OsString::from(format!(
                ".axial-repair-{}.tmp",
                uuid::Uuid::new_v4().simple()
            ));
            let file = open_relative(
                &self.parent,
                &name,
                Some(false),
                GENERIC_READ | GENERIC_WRITE | DELETE,
                FILE_SHARE_READ | FILE_SHARE_DELETE,
                FILE_CREATE,
            )?;
            let control = file.try_clone()?;
            let id = query::<FILE_ID_INFO>(&control, FileIdInfo)?;
            Ok(Temp {
                writer: Some(file),
                control,
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
            })
        }

        pub(super) fn remove_temp(&self, temp: Temp) {
            let mut disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
            unsafe {
                SetFileInformationByHandle(
                    temp.control.as_raw_handle() as HANDLE,
                    FileDispositionInfo,
                    (&mut disposition as *mut FILE_DISPOSITION_INFO).cast(),
                    size_of::<FILE_DISPOSITION_INFO>() as u32,
                );
            }
        }

        pub(super) fn promote_temp(&self, temp: &Temp) -> io::Result<()> {
            let held = query::<FILE_ID_INFO>(&temp.control, FileIdInfo)?;
            if held.VolumeSerialNumber != temp.volume || held.FileId.Identifier != temp.id {
                return Err(identity_changed("anchored record temp changed"));
            }
            rename_relative(&temp.control, &self.parent, &self.leaf)?;
            let current = open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let current = query::<FILE_ID_INFO>(&current, FileIdInfo)?;
            if current.VolumeSerialNumber != temp.volume || current.FileId.Identifier != temp.id {
                return Err(identity_changed(
                    "anchored record promoted identity changed",
                ));
            }
            Ok(())
        }

        pub(super) fn open_regular(
            &self,
            mutation_compatible: bool,
        ) -> io::Result<Option<RegularFile>> {
            self.revalidate()?;
            let share_mode = if mutation_compatible {
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE
            } else {
                FILE_SHARE_READ
            };
            let access =
                GENERIC_READ | FILE_READ_ATTRIBUTES | if mutation_compatible { DELETE } else { 0 };
            let file = match open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                access,
                share_mode,
                FILE_OPEN,
            ) {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error),
            };
            let basic = query::<FILE_BASIC_INFO>(&file, FileBasicInfo)?;
            let standard = query::<FILE_STANDARD_INFO>(&file, FileStandardInfo)?;
            let id = query::<FILE_ID_INFO>(&file, FileIdInfo)?;
            if basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
                || standard.Directory
                || standard.EndOfFile < 0
                || standard.NumberOfLinks != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is not an exact regular file",
                ));
            }
            let file = RegularFile {
                parent: self.parent.clone(),
                leaf: self.leaf.clone(),
                file,
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
                size: standard.EndOfFile,
                modified: basic.LastWriteTime,
                changed: basic.ChangeTime,
                share_mode,
            };
            if !file.revalidate() {
                return Err(identity_changed(
                    "anchored record identity changed during admission",
                ));
            }
            Ok(Some(file))
        }
    }

    impl RegularFile {

        pub(super) fn verify_sha1(
            mut self,
            expected_sha1: &str,
            expected_size: u64,
        ) -> Option<Self> {
            let expected_size = i64::try_from(expected_size).ok()?;
            if self.size != expected_size {
                return None;
            }
            self.file.seek(SeekFrom::Start(0)).ok()?;
            let mut hasher = Sha1::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = self.file.read(&mut buffer).ok()?;
                if count == 0 {
                    break;
                }
                observed = observed.checked_add(count as u64)?;
                if observed > expected_size as u64 {
                    return None;
                }
                hasher.update(&buffer[..count]);
            }
            (observed == expected_size as u64
                && format!("{:x}", hasher.finalize()) == expected_sha1
                && self.revalidate())
            .then_some(self)
        }


        pub(super) fn revalidate(&self) -> bool {
            let Ok(held_basic) = query::<FILE_BASIC_INFO>(&self.file, FileBasicInfo) else {
                return false;
            };
            let Ok(held_standard) = query::<FILE_STANDARD_INFO>(&self.file, FileStandardInfo)
            else {
                return false;
            };
            let Ok(held_id) = query::<FILE_ID_INFO>(&self.file, FileIdInfo) else {
                return false;
            };
            if !self.matches(&held_basic, &held_standard, &held_id) {
                return false;
            }
            let current = match open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                FILE_READ_ATTRIBUTES,
                self.share_mode,
                FILE_OPEN,
            ) {
                Ok(current) => current,
                Err(_) => return false,
            };
            let Ok(current_basic) = query::<FILE_BASIC_INFO>(&current, FileBasicInfo) else {
                return false;
            };
            let Ok(current_standard) = query::<FILE_STANDARD_INFO>(&current, FileStandardInfo)
            else {
                return false;
            };
            let Ok(current_id) = query::<FILE_ID_INFO>(&current, FileIdInfo) else {
                return false;
            };
            self.matches(&current_basic, &current_standard, &current_id)
        }

        fn reseal_after_rename(&mut self) -> io::Result<()> {
            let basic = query::<FILE_BASIC_INFO>(&self.file, FileBasicInfo)?;
            let standard = query::<FILE_STANDARD_INFO>(&self.file, FileStandardInfo)?;
            let id = query::<FILE_ID_INFO>(&self.file, FileIdInfo)?;
            if basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
                || standard.Directory
                || standard.NumberOfLinks != 1
                || id.VolumeSerialNumber != self.volume
                || id.FileId.Identifier != self.id
                || standard.EndOfFile != self.size
                || basic.LastWriteTime != self.modified
            {
                return Err(identity_changed(
                    "anchored record changed while it was being renamed",
                ));
            }
            self.changed = basic.ChangeTime;
            Ok(())
        }

        fn held_is_current(&self) -> bool {
            let Ok(basic) = query::<FILE_BASIC_INFO>(&self.file, FileBasicInfo) else {
                return false;
            };
            let Ok(standard) = query::<FILE_STANDARD_INFO>(&self.file, FileStandardInfo) else {
                return false;
            };
            let Ok(id) = query::<FILE_ID_INFO>(&self.file, FileIdInfo) else {
                return false;
            };
            self.matches(&basic, &standard, &id)
        }

        fn matches(
            &self,
            basic: &FILE_BASIC_INFO,
            standard: &FILE_STANDARD_INFO,
            id: &FILE_ID_INFO,
        ) -> bool {
            basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) == 0
                && !standard.Directory
                && standard.NumberOfLinks == 1
                && standard.EndOfFile == self.size
                && basic.LastWriteTime == self.modified
                && basic.ChangeTime == self.changed
                && id.VolumeSerialNumber == self.volume
                && id.FileId.Identifier == self.id
        }
    }

    impl ExactRenameReceipt {
        pub(super) fn revalidate(&self) -> io::Result<()> {
            self.leaf.revalidate()?;
            self.leaf
                .revalidate_mutation_parent(&self.mutation_parent)?;
            if !self.file.held_is_current() || !self.leaf.target_is_missing()? {
                return Err(identity_changed(
                    "anchored record rename receipt is no longer current",
                ));
            }
            let destination = open_relative(
                &self.mutation_parent,
                &self.destination,
                Some(false),
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let basic = query::<FILE_BASIC_INFO>(&destination, FileBasicInfo)?;
            let standard = query::<FILE_STANDARD_INFO>(&destination, FileStandardInfo)?;
            let id = query::<FILE_ID_INFO>(&destination, FileIdInfo)?;
            if !self.file.matches(&basic, &standard, &id) {
                return Err(identity_changed(
                    "anchored record rename destination changed identity",
                ));
            }
            self.leaf.revalidate()
        }
    }

    fn open_absolute_directory_chain(path: &Path) -> io::Result<(PathBuf, Vec<HeldDirectory>)> {
        let mut components = path.components();
        let Component::Prefix(prefix) = components.next().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record root is empty",
            )
        })?
        else {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record root is not drive-absolute",
            ));
        };
        if !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
            || components.next() != Some(Component::RootDir)
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record root is not a supported drive path",
            ));
        }
        let mut anchor_path = PathBuf::from(prefix.as_os_str());
        anchor_path.push(Path::new(r"\"));
        let root = Arc::new(open_root_exact(&anchor_path)?);
        let root_id = query::<FILE_ID_INFO>(&root, FileIdInfo)?;
        let mut directories = vec![HeldDirectory {
            handle: root.clone(),
            parent: None,
            name: None,
            volume: root_id.VolumeSerialNumber,
            id: root_id.FileId.Identifier,
        }];
        let mut parent = root;
        for component in components {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record root is not normalized",
                ));
            };
            let child = Arc::new(open_relative(
                &parent,
                name,
                Some(true),
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
            )?);
            require_exact_directory(&child)?;
            let id = query::<FILE_ID_INFO>(&child, FileIdInfo)?;
            directories.push(HeldDirectory {
                handle: child.clone(),
                parent: Some(parent),
                name: Some(name.to_os_string()),
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
            });
            parent = child;
        }
        Ok((anchor_path, directories))
    }

    fn revalidate_directory_chain(
        root_path: &Path,
        directories: &[HeldDirectory],
    ) -> io::Result<()> {
        let root = open_root_exact(root_path)?;
        let root_id = query::<FILE_ID_INFO>(&root, FileIdInfo)?;
        let expected = &directories[0];
        if root_id.VolumeSerialNumber != expected.volume || root_id.FileId.Identifier != expected.id
        {
            return Err(identity_changed("anchored record root changed"));
        }
        let held_root = query::<FILE_ID_INFO>(&expected.handle, FileIdInfo)?;
        if held_root.VolumeSerialNumber != expected.volume
            || held_root.FileId.Identifier != expected.id
        {
            return Err(identity_changed("anchored record held root changed"));
        }
        for directory in directories.iter().skip(1) {
            let current = open_relative(
                directory.parent.as_ref().expect("child has parent"),
                directory.name.as_ref().expect("child has name"),
                Some(true),
                FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
            )?;
            require_exact_directory(&current)?;
            let current_id = query::<FILE_ID_INFO>(&current, FileIdInfo)?;
            let held_id = query::<FILE_ID_INFO>(&directory.handle, FileIdInfo)?;
            if current_id.VolumeSerialNumber != directory.volume
                || current_id.FileId.Identifier != directory.id
                || held_id.VolumeSerialNumber != directory.volume
                || held_id.FileId.Identifier != directory.id
            {
                return Err(identity_changed("anchored record ancestor changed"));
            }
        }
        Ok(())
    }


    fn require_direct_leaf(name: &OsStr) -> io::Result<()> {
        let encoded = name.encode_wide().collect::<Vec<_>>();
        if encoded.is_empty()
            || encoded.len() > super::MAX_DIRECT_LEAF_UNITS
            || encoded.contains(&0)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "anchored record name exceeds the supported direct-leaf bound",
            ));
        }
        let mut components = Path::new(name).components();
        if !matches!(components.next(), Some(Component::Normal(component)) if component == name)
            || components.next().is_some()
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record name is not a direct leaf",
            ));
        }
        Ok(())
    }

    fn require_target_missing(parent: &fs::File, name: &OsStr) -> io::Result<()> {
        match open_relative(
            parent,
            name,
            None,
            FILE_READ_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
        ) {
            Ok(_) => Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "anchored record rename destination exists",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn query<T: Default>(file: &fs::File, class: i32) -> io::Result<T> {
        let mut value = T::default();
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle() as HANDLE,
                class,
                (&mut value as *mut T).cast(),
                size_of::<T>() as u32,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }

    fn open_root_exact(root: &Path) -> io::Result<fs::File> {
        open_root_exact_with_access(root, FILE_READ_ATTRIBUTES | FILE_TRAVERSE)
    }

    fn open_root_exact_with_access(root: &Path, access: u32) -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
        let file = options.open(root)?;
        require_exact_directory(&file)?;
        Ok(file)
    }

    fn require_exact_directory(file: &fs::File) -> io::Result<()> {
        let basic = query::<FILE_BASIC_INFO>(file, FileBasicInfo)?;
        let standard = query::<FILE_STANDARD_INFO>(file, FileStandardInfo)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || !standard.Directory
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record ancestor is not an exact directory",
            ));
        }
        Ok(())
    }

    fn open_relative(
        parent: &fs::File,
        name: &OsStr,
        directory: Option<bool>,
        access: u32,
        share: u32,
        disposition: u32,
    ) -> io::Result<fs::File> {
        let mut encoded = name.encode_wide().collect::<Vec<_>>();
        if encoded.is_empty() || encoded.len() > (u16::MAX as usize / 2) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid relative leaf",
            ));
        }
        let mut unicode = UNICODE_STRING {
            Length: (encoded.len() * 2) as u16,
            MaximumLength: (encoded.len() * 2) as u16,
            Buffer: encoded.as_mut_ptr(),
        };
        let attributes = OBJECT_ATTRIBUTES {
            Length: size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: parent.as_raw_handle() as HANDLE,
            ObjectName: &mut unicode,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null_mut(),
            SecurityQualityOfService: ptr::null_mut(),
        };
        let mut status = IO_STATUS_BLOCK::default();
        let mut handle: HANDLE = ptr::null_mut();
        let type_option = match directory {
            Some(true) => FILE_DIRECTORY_FILE,
            Some(false) => FILE_NON_DIRECTORY_FILE,
            None => 0,
        };
        let result = unsafe {
            NtCreateFile(
                &mut handle,
                access | FILE_READ_ATTRIBUTES | SYNCHRONIZE,
                &attributes,
                &mut status,
                ptr::null(),
                0,
                share,
                disposition,
                FILE_OPEN_REPARSE_POINT | FILE_SYNCHRONOUS_IO_NONALERT | type_option,
                ptr::null(),
                0,
            )
        };
        if result < 0 {
            if !handle.is_null() {
                unsafe { CloseHandle(handle) };
            }
            let code = unsafe { RtlNtStatusToDosError(result) };
            return Err(io::Error::from_raw_os_error(code as i32));
        }
        Ok(unsafe { fs::File::from_raw_handle(handle) })
    }

    fn rename_relative(file: &fs::File, parent: &fs::File, name: &OsStr) -> io::Result<()> {
        let encoded = name.encode_wide().collect::<Vec<_>>();
        let name_bytes = encoded
            .len()
            .checked_mul(size_of::<u16>())
            .ok_or_else(|| io::Error::other("rename target too long"))?;
        let buffer_size = size_of::<FILE_RENAME_INFORMATION>()
            .checked_add(name_bytes)
            .ok_or_else(|| io::Error::other("rename buffer overflow"))?;
        let mut buffer = vec![0_usize; buffer_size.div_ceil(size_of::<usize>())];
        let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFORMATION>();
        unsafe {
            (*info).Anonymous.ReplaceIfExists = false;
            (*info).RootDirectory = parent.as_raw_handle() as HANDLE;
            (*info).FileNameLength = name_bytes
                .try_into()
                .map_err(|_| io::Error::other("rename target too long"))?;
            ptr::copy_nonoverlapping(
                encoded.as_ptr(),
                (*info).FileName.as_mut_ptr(),
                encoded.len(),
            );
            let mut status = IO_STATUS_BLOCK::default();
            let result = NtSetInformationFile(
                file.as_raw_handle() as HANDLE,
                &mut status,
                info.cast(),
                buffer_size
                    .try_into()
                    .map_err(|_| io::Error::other("rename buffer too large"))?,
                FileRenameInformation,
            );
            if result < 0 {
                let code = RtlNtStatusToDosError(result);
                return Err(io::Error::from_raw_os_error(code as i32));
            }
        }
        Ok(())
    }

    fn identity_changed(message: &'static str) -> io::Error {
        io::Error::new(io::ErrorKind::PermissionDenied, message)
    }

    #[cfg(test)]
    mod normalization_tests {
        use super::require_direct_leaf;
        use std::ffi::OsStr;

        #[test]
        fn direct_leaf_bound_counts_windows_utf16_units() {
            let surrogate_pair_at_bound = "x".repeat(253) + "😀";
            let surrogate_pair_over_bound = "x".repeat(254) + "😀";
            assert!(require_direct_leaf(OsStr::new(&"x".repeat(255))).is_ok());
            assert!(require_direct_leaf(OsStr::new(&"x".repeat(256))).is_err());
            assert!(require_direct_leaf(OsStr::new(&surrogate_pair_at_bound)).is_ok());
            assert!(require_direct_leaf(OsStr::new(&surrogate_pair_over_bound)).is_err());
        }
    }
}



#[cfg(not(any(unix, windows)))]
mod platform {
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::Path;
    pub(super) struct Leaf;
    pub(super) struct RegularFile;
    pub(super) struct ExactRenameReceipt;
    pub(super) struct Temp;


    impl Leaf {
        pub(super) fn open(_root: &Path, _relative: &Path) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }


        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            self.revalidate().map(|()| false)
        }

        pub(super) fn rename_exact(
            &self,
            _file: RegularFile,
            _destination: OsString,
        ) -> Result<ExactRenameReceipt, super::RegisteredArtifactQuarantineError> {
            Err(super::RegisteredArtifactQuarantineError::Refused(
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "anchored record rename is unavailable on this platform",
                ),
            ))
        }

        pub(super) fn create_temp(&self) -> io::Result<Temp> {
            self.revalidate().map(|()| Temp)
        }

        pub(super) fn remove_temp(&self, _temp: Temp) {}

        pub(super) fn promote_temp(&self, _temp: &Temp) -> io::Result<()> {
            self.revalidate()
        }

        pub(super) fn open_regular(
            &self,
            _mutation_compatible: bool,
        ) -> io::Result<Option<RegularFile>> {
            self.revalidate().map(|()| None)
        }
    }

    impl RegularFile {

        pub(super) fn verify_sha1(self, _expected_sha1: &str, _expected_size: u64) -> Option<Self> {
            None
        }


        pub(super) fn revalidate(&self) -> bool {
            false
        }
    }

    impl Temp {
        pub(super) fn take_writer(&mut self) -> Option<std::fs::File> {
            None
        }
    }

    impl ExactRenameReceipt {
        pub(super) fn revalidate(&self) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored record rename is unavailable on this platform",
            ))
        }
    }
}
