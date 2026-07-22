//! Identity-bound access to exact regular files below held no-follow directories.

use std::ffi::{OsStr, OsString};
use std::io;
use std::io::Read as _;
use std::sync::Arc;

#[cfg(test)]
use std::path::Path;

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
