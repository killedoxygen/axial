//! Identity-bound access to exact regular files below held no-follow directories.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

use sha2::{Digest as _, Sha256};

const RESTART_IDENTITY_DOMAIN: &[u8] = b"axial.persisted-state-restart-record-identity.v2\0";
const RESTART_IDENTITY_EDGE_SAMPLE_BYTES: usize = 4 * 1024;

#[path = "registered_artifact.rs"]
pub(crate) mod registered_artifact;

pub(crate) struct AnchoredRecordIdentity {
    leaf: AnchoredLeaf,
    file: AnchoredRegularFile,
}

pub(crate) struct AnchoredRecordQuarantineReceipt {
    exact: platform::ExactRenameReceipt,
}

pub(crate) enum AnchoredRecordQuarantineError {
    Refused(io::Error),
    AppliedUnverified(io::Error),
}

pub(crate) struct AnchoredRecordRestartDigest([u8; 32]);

pub(crate) struct AnchoredRecordDirectory(platform::Directory);

#[derive(Eq, PartialEq)]
pub(crate) struct AnchoredRecordDirectoryEpoch(platform::DirectoryEpoch);

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

#[derive(Clone, Copy)]
enum ExactRenameTestStage {
    BeforeFinalPrecheck,
    AfterRename,
    AfterSync,
}

#[cfg(test)]
type ExactRenameTestHook = Option<Box<dyn FnOnce()>>;

#[cfg(test)]
thread_local! {
    static EXACT_RENAME_TEST_HOOKS: std::cell::RefCell<[ExactRenameTestHook; 3]> =
        std::cell::RefCell::new([None, None, None]);
}

#[cfg(test)]
fn set_exact_rename_test_hook(stage: ExactRenameTestStage, hook: impl FnOnce() + 'static) {
    EXACT_RENAME_TEST_HOOKS.with(|hooks| {
        hooks.borrow_mut()[stage as usize] = Some(Box::new(hook));
    });
}

#[cfg(test)]
fn run_exact_rename_test_hook(stage: ExactRenameTestStage) {
    EXACT_RENAME_TEST_HOOKS.with(|hooks| {
        if let Some(hook) = hooks.borrow_mut()[stage as usize].take() {
            hook();
        }
    });
}

#[cfg(not(test))]
fn run_exact_rename_test_hook(_stage: ExactRenameTestStage) {}

impl AnchoredRecordObservation {
    #[cfg(test)]
    pub(crate) fn read(root: &Path, relative: &Path, max_bytes: u64) -> io::Result<Self> {
        let leaf = AnchoredLeaf::open(root, relative)?;
        Self::read_leaf(leaf, max_bytes, false)
    }

    fn read_leaf(
        leaf: AnchoredLeaf,
        max_bytes: u64,
        mutation_compatible: bool,
    ) -> io::Result<Self> {
        let file = leaf
            .open_regular_with_intent(mutation_compatible)?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "anchored record is missing"))?;
        if file.size() > max_bytes {
            let identity = AnchoredRecordIdentity { leaf, file };
            identity.revalidate()?;
            return Ok(Self::Oversized { identity });
        }
        let (bytes, file) = file.read_bounded(max_bytes)?;
        let identity = AnchoredRecordIdentity { leaf, file };
        identity.revalidate()?;
        Ok(Self::Bytes { bytes, identity })
    }

    pub(crate) fn bytes(&self) -> Option<&[u8]> {
        match self {
            Self::Bytes { bytes, .. } => Some(bytes),
            Self::Oversized { .. } => None,
        }
    }

    pub(crate) fn is_oversized(&self) -> bool {
        matches!(self, Self::Oversized { .. })
    }

    pub(super) fn revalidate(&self) -> io::Result<()> {
        match self {
            Self::Bytes { identity, .. } | Self::Oversized { identity } => identity.revalidate(),
        }
    }

    pub(super) fn into_bytes(self) -> Option<Vec<u8>> {
        match self {
            Self::Bytes { bytes, .. } => Some(bytes),
            Self::Oversized { .. } => None,
        }
    }

    #[cfg(test)]
    pub(crate) fn into_identity(self) -> AnchoredRecordIdentity {
        match self {
            Self::Bytes { identity, .. } | Self::Oversized { identity } => identity,
        }
    }

    pub(crate) fn into_restart_identity(
        self,
    ) -> io::Result<(AnchoredRecordIdentity, AnchoredRecordRestartDigest)> {
        let (mut identity, bytes) = match self {
            Self::Bytes { bytes, identity } => (identity, Some(bytes)),
            Self::Oversized { identity } => (identity, None),
        };
        identity.revalidate()?;
        let mut hasher = Sha256::new();
        hasher.update(RESTART_IDENTITY_DOMAIN);
        identity.leaf.update_restart_identity(&mut hasher);
        identity
            .file
            .update_restart_identity(&mut hasher, bytes.as_deref())?;
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

    #[cfg(test)]
    fn bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl AnchoredRecordDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        platform::Directory::open(path).map(Self)
    }

    pub(crate) fn names(&self) -> io::Result<Vec<OsString>> {
        self.0.names()
    }

    pub(crate) fn names_bounded(&self, max_entries: usize) -> io::Result<Option<Vec<OsString>>> {
        self.0.names_bounded(max_entries)
    }

    pub(crate) fn epoch(&self) -> io::Result<AnchoredRecordDirectoryEpoch> {
        self.0.epoch().map(AnchoredRecordDirectoryEpoch)
    }

    pub(crate) fn read(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordObservation> {
        let leaf = self.0.open_leaf(name).map(AnchoredLeaf)?;
        AnchoredRecordObservation::read_leaf(leaf, max_bytes, false)
    }

    pub(crate) fn digest(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordDigestObservation> {
        let leaf = self.0.open_leaf(name).map(AnchoredLeaf)?;
        let file = leaf
            .open_regular()?
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "anchored record is missing"))?;
        if file.size() > max_bytes {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "anchored record exceeds its digest bound",
            ));
        }
        let size = file.size();
        let modified_at_ns = file.modified_at_ns()?;
        let (sha256, sha512, file) = file.digest_bounded(max_bytes)?;
        let identity = AnchoredRecordIdentity { leaf, file };
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
        let leaf = self.0.open_leaf(name).map(AnchoredLeaf)?;
        AnchoredRecordObservation::read_leaf(leaf, max_bytes, true)
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
        self.leaf.revalidate()?;
        if !self.file.revalidate() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "anchored record identity changed",
            ));
        }
        self.leaf.revalidate()
    }

    pub(crate) fn quarantine(
        self,
        suffix: [u8; 16],
    ) -> Result<AnchoredRecordQuarantineReceipt, AnchoredRecordQuarantineError> {
        let destination = anchored_record_quarantine_name(self.leaf.name(), suffix);
        self.leaf
            .rename_exact(self.file, destination)
            .map(|exact| AnchoredRecordQuarantineReceipt { exact })
    }

    #[cfg(test)]
    pub(crate) fn is_current(&self) -> bool {
        self.revalidate().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn same_file(&self, other: &Self) -> bool {
        self.file.same_identity(&other.file)
    }
}

impl AnchoredRecordQuarantineReceipt {
    pub(crate) fn is_current(&self) -> bool {
        self.exact.revalidate().is_ok()
    }
}

impl AnchoredRecordQuarantineError {
    fn into_io_error(self) -> io::Error {
        match self {
            Self::Refused(error) | Self::AppliedUnverified(error) => error,
        }
    }
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

    fn name(&self) -> &OsStr {
        self.0.name()
    }

    fn update_restart_identity(&self, hasher: &mut Sha256) {
        self.0.update_restart_identity(hasher);
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
            .map_err(AnchoredRecordQuarantineError::into_io_error)
    }

    fn rename_exact(
        &self,
        file: AnchoredRegularFile,
        destination: OsString,
    ) -> Result<platform::ExactRenameReceipt, AnchoredRecordQuarantineError> {
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
    fn size(&self) -> u64 {
        self.0.size()
    }

    fn modified_at_ns(&self) -> io::Result<u64> {
        self.0.modified_at_ns()
    }

    fn verify_sha1(self, expected_sha1: &str, expected_size: u64) -> Option<Self> {
        self.0.verify_sha1(expected_sha1, expected_size).map(Self)
    }

    fn read_bounded(self, max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
        self.0
            .read_bounded(max_bytes)
            .map(|(bytes, file)| (bytes, Self(file)))
    }

    fn digest_bounded(self, max_bytes: u64) -> io::Result<([u8; 32], [u8; 64], Self)> {
        self.0
            .digest_bounded(max_bytes)
            .map(|(sha256, sha512, file)| (sha256, sha512, Self(file)))
    }

    fn update_restart_identity(
        &mut self,
        hasher: &mut Sha256,
        full_bytes: Option<&[u8]>,
    ) -> io::Result<()> {
        self.0.update_restart_identity(hasher, full_bytes)
    }

    fn revalidate(&self) -> bool {
        self.0.revalidate()
    }

    #[cfg(test)]
    fn same_identity(&self, other: &Self) -> bool {
        self.0.same_identity(&other.0)
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
    use sha2::{Sha256, Sha512};
    use std::ffi::{OsStr, OsString};
    use std::io::{self, Read as _, Seek as _, SeekFrom};
    use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
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

    pub(super) struct Directory {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<OwnedFd>,
    }

    #[derive(Eq, PartialEq)]
    pub(super) struct DirectoryEpoch {
        device: u64,
        inode: u64,
        modified_seconds: i64,
        modified_nanoseconds: u64,
        changed_seconds: i64,
        changed_nanoseconds: u64,
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

    impl Directory {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            let root_path = PathBuf::from("/");
            let directories = open_absolute_directory_chain(path)?;
            let parent = directories
                .last()
                .expect("absolute directory chain has anchor")
                .handle
                .clone();
            let directory = Self {
                root_path,
                directories,
                parent,
            };
            directory.revalidate()?;
            Ok(directory)
        }

        pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
            self.names_bounded(usize::MAX)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration overflowed",
                )
            })
        }

        pub(super) fn names_bounded(
            &self,
            max_entries: usize,
        ) -> io::Result<Option<Vec<OsString>>> {
            self.revalidate()?;
            let entries = rustix::fs::Dir::read_from(self.parent.as_ref())?;
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    if names.len() == max_entries {
                        return Ok(None);
                    }
                    names.push(OsString::from_vec(name.to_vec()));
                }
            }
            self.revalidate()?;
            Ok(Some(names))
        }

        pub(super) fn epoch(&self) -> io::Result<DirectoryEpoch> {
            self.revalidate()?;
            let stat = rustix::fs::fstat(self.parent.as_ref()).map_err(io::Error::from)?;
            let epoch = DirectoryEpoch {
                device: canonical_unsigned(stat.st_dev, "directory device")?,
                inode: canonical_unsigned(stat.st_ino, "directory inode")?,
                modified_seconds: canonical_signed(stat.st_mtime, "directory modified seconds")?,
                modified_nanoseconds: canonical_nanoseconds(
                    stat.st_mtime_nsec,
                    "directory modified nanoseconds",
                )?,
                changed_seconds: canonical_signed(stat.st_ctime, "directory changed seconds")?,
                changed_nanoseconds: canonical_nanoseconds(
                    stat.st_ctime_nsec,
                    "directory changed nanoseconds",
                )?,
            };
            self.revalidate()?;
            Ok(epoch)
        }

        pub(super) fn open_leaf(&self, name: &OsStr) -> io::Result<Leaf> {
            require_direct_leaf(name)?;
            self.revalidate()?;
            Ok(Leaf {
                root_path: self.root_path.clone(),
                directories: self.directories.clone(),
                parent: self.parent.clone(),
                leaf: name.to_os_string(),
            })
        }

        fn revalidate(&self) -> io::Result<()> {
            revalidate_directory_chain(&self.root_path, &self.directories)
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

        pub(super) fn name(&self) -> &OsStr {
            &self.leaf
        }

        pub(super) fn update_restart_identity(&self, hasher: &mut Sha256) {
            hasher.update(b"unix-directory-chain-v1\0");
            hasher.update((self.directories.len() as u64).to_le_bytes());
            for directory in &self.directories {
                hasher.update(directory.identity.device.to_le_bytes());
                hasher.update(directory.identity.inode.to_le_bytes());
            }
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
        ) -> Result<ExactRenameReceipt, super::AnchoredRecordQuarantineError> {
            use super::AnchoredRecordQuarantineError::{AppliedUnverified, Refused};

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
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::BeforeFinalPrecheck);
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
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::AfterRename);
            file.reseal_after_rename().map_err(AppliedUnverified)?;
            let receipt = ExactRenameReceipt {
                leaf: self.clone(),
                destination,
                file,
            };
            receipt.revalidate().map_err(AppliedUnverified)?;
            rustix::fs::fsync(self.parent.as_ref())
                .map_err(|error| AppliedUnverified(io::Error::from(error)))?;
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::AfterSync);
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

    #[cfg(test)]
    pub(super) fn exact_noreplace_rename_supported() -> bool {
        require_exact_noreplace_rename().is_ok()
    }

    impl RegularFile {
        pub(super) fn size(&self) -> u64 {
            self.metadata.size
        }

        pub(super) fn modified_at_ns(&self) -> io::Result<u64> {
            if self.metadata.modified_seconds < 0
                || self.metadata.modified_nanoseconds >= 1_000_000_000
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored record modified time is invalid",
                ));
            }
            u64::try_from(self.metadata.modified_seconds)
                .ok()
                .and_then(|seconds| seconds.checked_mul(1_000_000_000))
                .and_then(|value| value.checked_add(self.metadata.modified_nanoseconds))
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored record modified time overflowed",
                    )
                })
        }

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

        pub(super) fn read_bounded(mut self, max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
            if self.metadata.size > max_bytes || !self.revalidate() {
                return Err(identity_changed(
                    "anchored record changed or exceeds its read bound",
                ));
            }
            self.file.seek(SeekFrom::Start(0))?;
            let capacity = usize::try_from(self.metadata.size).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored record size overflowed",
                )
            })?;
            let mut bytes = Vec::with_capacity(capacity);
            self.file
                .by_ref()
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != self.metadata.size || !self.revalidate() {
                return Err(identity_changed("anchored record changed during reading"));
            }
            Ok((bytes, self))
        }

        pub(super) fn digest_bounded(
            mut self,
            max_bytes: u64,
        ) -> io::Result<([u8; 32], [u8; 64], Self)> {
            if self.metadata.size > max_bytes || !self.revalidate() {
                return Err(identity_changed(
                    "anchored record changed or exceeds its digest bound",
                ));
            }
            self.file.seek(SeekFrom::Start(0))?;
            let mut sha256 = Sha256::new();
            let mut sha512 = Sha512::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = self.file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                observed = observed.checked_add(count as u64).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored digest size overflowed",
                    )
                })?;
                if observed > self.metadata.size || observed > max_bytes {
                    return Err(identity_changed("anchored record grew during digesting"));
                }
                sha256.update(&buffer[..count]);
                sha512.update(&buffer[..count]);
            }
            if observed != self.metadata.size || !self.revalidate() {
                return Err(identity_changed("anchored record changed during digesting"));
            }
            Ok((sha256.finalize().into(), sha512.finalize().into(), self))
        }

        pub(super) fn update_restart_identity(
            &mut self,
            hasher: &mut Sha256,
            full_bytes: Option<&[u8]>,
        ) -> io::Result<()> {
            hasher.update(b"unix-regular-file-v2\0");
            hasher.update(self.metadata.identity.device.to_le_bytes());
            hasher.update(self.metadata.identity.inode.to_le_bytes());
            hasher.update(self.metadata.size.to_le_bytes());
            hasher.update(self.metadata.modified_seconds.to_le_bytes());
            hasher.update(self.metadata.modified_nanoseconds.to_le_bytes());
            match full_bytes {
                Some(bytes) => {
                    if u64::try_from(bytes.len()).ok() != Some(self.metadata.size) {
                        return Err(identity_changed(
                            "anchored record bytes do not match held identity",
                        ));
                    }
                    hasher.update(b"full-record-v1\0");
                    hasher.update(self.metadata.size.to_le_bytes());
                    hasher.update(bytes);
                }
                None => {
                    let sample_len = super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES;
                    let tail_offset = self
                        .metadata
                        .size
                        .checked_sub(sample_len as u64)
                        .ok_or_else(|| {
                            io::Error::new(
                                io::ErrorKind::InvalidData,
                                "oversized anchored record is too short for fixed edge samples",
                            )
                        })?;
                    let mut head = [0_u8; super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES];
                    let mut tail = [0_u8; super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES];
                    self.file.seek(SeekFrom::Start(0))?;
                    self.file.read_exact(&mut head)?;
                    self.file.seek(SeekFrom::Start(tail_offset))?;
                    self.file.read_exact(&mut tail)?;
                    hasher.update(b"fixed-edge-samples-v1\0");
                    hasher.update((sample_len as u64).to_le_bytes());
                    hasher.update(head);
                    hasher.update((sample_len as u64).to_le_bytes());
                    hasher.update(tail);
                }
            }
            Ok(())
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

        #[cfg(test)]
        pub(super) fn same_identity(&self, other: &Self) -> bool {
            self.metadata.identity == other.metadata.identity
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
        if name.as_bytes().is_empty() || name.as_bytes().len() > libc::NAME_MAX as usize {
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
        use super::{canonical_nanoseconds, canonical_signed, canonical_unsigned};

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
    use sha2::{Sha256, Sha512};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io::{self, Read as _, Seek as _, SeekFrom};
    use std::mem::{offset_of, size_of};
    use std::os::windows::ffi::{OsStrExt as _, OsStringExt as _};
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
        CloseHandle, ERROR_NO_MORE_FILES, GENERIC_READ, GENERIC_WRITE, HANDLE,
        OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        DELETE, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
        FILE_ID_BOTH_DIR_INFO, FILE_ID_INFO, FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FILE_TRAVERSE,
        FileBasicInfo, FileDispositionInfo, FileIdBothDirectoryInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx, SYNCHRONIZE, SetFileInformationByHandle,
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

    pub(super) struct Directory {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<fs::File>,
    }

    #[derive(Eq, PartialEq)]
    pub(super) struct DirectoryEpoch {
        volume: u64,
        id: [u8; 16],
        modified: i64,
        changed: i64,
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

    impl Directory {
        pub(super) fn open(path: &Path) -> io::Result<Self> {
            let (root_path, directories) = open_absolute_directory_chain(path)?;
            let expected = directories
                .last()
                .expect("absolute directory chain has anchor");
            let parent = Arc::new(match (&expected.parent, &expected.name) {
                (Some(parent), Some(name)) => open_relative(
                    parent,
                    name,
                    Some(true),
                    FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
                )?,
                (None, None) => open_root_exact_with_access(
                    &root_path,
                    FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES | FILE_TRAVERSE,
                )?,
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored directory chain is incoherent",
                    ));
                }
            });
            require_exact_directory(&parent)?;
            let listed_id = query::<FILE_ID_INFO>(&parent, FileIdInfo)?;
            if listed_id.VolumeSerialNumber != expected.volume
                || listed_id.FileId.Identifier != expected.id
            {
                return Err(identity_changed(
                    "anchored directory listing handle changed identity",
                ));
            }
            let directory = Self {
                root_path,
                directories,
                parent,
            };
            directory.revalidate()?;
            Ok(directory)
        }

        pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
            self.names_bounded(usize::MAX)?.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "directory enumeration overflowed",
                )
            })
        }

        pub(super) fn names_bounded(
            &self,
            max_entries: usize,
        ) -> io::Result<Option<Vec<OsString>>> {
            self.revalidate()?;
            let mut names = Vec::new();
            let mut buffer = vec![0_u64; (64 * 1024) / size_of::<u64>()];
            loop {
                let ok = unsafe {
                    GetFileInformationByHandleEx(
                        self.parent.as_raw_handle() as HANDLE,
                        FileIdBothDirectoryInfo,
                        buffer.as_mut_ptr().cast(),
                        (buffer.len() * size_of::<u64>()) as u32,
                    )
                };
                if ok == 0 {
                    let error = io::Error::last_os_error();
                    if error.raw_os_error() == Some(ERROR_NO_MORE_FILES as i32) {
                        break;
                    }
                    return Err(error);
                }
                collect_directory_names(&buffer, &mut names)?;
                if names.len() > max_entries {
                    return Ok(None);
                }
            }
            self.revalidate()?;
            Ok(Some(names))
        }

        pub(super) fn epoch(&self) -> io::Result<DirectoryEpoch> {
            self.revalidate()?;
            let basic = query::<FILE_BASIC_INFO>(&self.parent, FileBasicInfo)?;
            let id = query::<FILE_ID_INFO>(&self.parent, FileIdInfo)?;
            let epoch = DirectoryEpoch {
                volume: id.VolumeSerialNumber,
                id: id.FileId.Identifier,
                modified: basic.LastWriteTime,
                changed: basic.ChangeTime,
            };
            self.revalidate()?;
            Ok(epoch)
        }

        pub(super) fn open_leaf(&self, name: &OsStr) -> io::Result<Leaf> {
            require_direct_leaf(name)?;
            self.revalidate()?;
            Ok(Leaf {
                root_path: self.root_path.clone(),
                directories: self.directories.clone(),
                parent: self.parent.clone(),
                leaf: name.to_os_string(),
            })
        }

        fn revalidate(&self) -> io::Result<()> {
            revalidate_directory_chain(&self.root_path, &self.directories)?;
            let expected = self
                .directories
                .last()
                .expect("absolute directory chain has anchor");
            let listed_id = query::<FILE_ID_INFO>(&self.parent, FileIdInfo)?;
            if listed_id.VolumeSerialNumber != expected.volume
                || listed_id.FileId.Identifier != expected.id
            {
                return Err(identity_changed(
                    "anchored directory listing handle changed identity",
                ));
            }
            Ok(())
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

        pub(super) fn name(&self) -> &OsStr {
            &self.leaf
        }

        pub(super) fn update_restart_identity(&self, hasher: &mut Sha256) {
            hasher.update(b"windows-directory-chain-v1\0");
            hasher.update((self.directories.len() as u64).to_le_bytes());
            for directory in &self.directories {
                hasher.update(directory.volume.to_le_bytes());
                hasher.update(directory.id);
            }
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
        ) -> Result<ExactRenameReceipt, super::AnchoredRecordQuarantineError> {
            use super::AnchoredRecordQuarantineError::{AppliedUnverified, Refused};

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
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::BeforeFinalPrecheck);
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
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::AfterRename);
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
            super::run_exact_rename_test_hook(super::ExactRenameTestStage::AfterSync);
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
        pub(super) fn size(&self) -> u64 {
            self.size
                .try_into()
                .expect("validated record size is nonnegative")
        }

        pub(super) fn modified_at_ns(&self) -> io::Result<u64> {
            const WINDOWS_TO_UNIX_EPOCH_TICKS: i64 = 116_444_736_000_000_000;
            u64::try_from(
                self.modified
                    .checked_sub(WINDOWS_TO_UNIX_EPOCH_TICKS)
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "anchored record modified time predates the Unix epoch",
                        )
                    })?,
            )
            .ok()
            .and_then(|ticks| ticks.checked_mul(100))
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored record modified time overflowed",
                )
            })
        }

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

        pub(super) fn read_bounded(mut self, max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
            let size = self.size();
            if size > max_bytes || !self.revalidate() {
                return Err(identity_changed(
                    "anchored record changed or exceeds its read bound",
                ));
            }
            self.file.seek(SeekFrom::Start(0))?;
            let capacity = usize::try_from(size).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored record size overflowed",
                )
            })?;
            let mut bytes = Vec::with_capacity(capacity);
            self.file
                .by_ref()
                .take(max_bytes.saturating_add(1))
                .read_to_end(&mut bytes)?;
            if bytes.len() as u64 != size || !self.revalidate() {
                return Err(identity_changed("anchored record changed during reading"));
            }
            Ok((bytes, self))
        }

        pub(super) fn digest_bounded(
            mut self,
            max_bytes: u64,
        ) -> io::Result<([u8; 32], [u8; 64], Self)> {
            let size = self.size();
            if size > max_bytes || !self.revalidate() {
                return Err(identity_changed(
                    "anchored record changed or exceeds its digest bound",
                ));
            }
            self.file.seek(SeekFrom::Start(0))?;
            let mut sha256 = Sha256::new();
            let mut sha512 = Sha512::new();
            let mut observed = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let count = self.file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                observed = observed.checked_add(count as u64).ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored digest size overflowed",
                    )
                })?;
                if observed > size || observed > max_bytes {
                    return Err(identity_changed("anchored record grew during digesting"));
                }
                sha256.update(&buffer[..count]);
                sha512.update(&buffer[..count]);
            }
            if observed != size || !self.revalidate() {
                return Err(identity_changed("anchored record changed during digesting"));
            }
            Ok((sha256.finalize().into(), sha512.finalize().into(), self))
        }

        pub(super) fn update_restart_identity(
            &mut self,
            hasher: &mut Sha256,
            full_bytes: Option<&[u8]>,
        ) -> io::Result<()> {
            hasher.update(b"windows-regular-file-v2\0");
            hasher.update(self.volume.to_le_bytes());
            hasher.update(self.id);
            hasher.update(self.size.to_le_bytes());
            hasher.update(self.modified.to_le_bytes());
            match full_bytes {
                Some(bytes) => {
                    if usize::try_from(self.size).ok() != Some(bytes.len()) {
                        return Err(identity_changed(
                            "anchored record bytes do not match held identity",
                        ));
                    }
                    hasher.update(b"full-record-v1\0");
                    hasher.update(self.size.to_le_bytes());
                    hasher.update(bytes);
                }
                None => {
                    let sample_len = super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES;
                    let size = u64::try_from(self.size).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "anchored record size is invalid",
                        )
                    })?;
                    let tail_offset = size.checked_sub(sample_len as u64).ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidData,
                            "oversized anchored record is too short for fixed edge samples",
                        )
                    })?;
                    let mut head = [0_u8; super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES];
                    let mut tail = [0_u8; super::RESTART_IDENTITY_EDGE_SAMPLE_BYTES];
                    self.file.seek(SeekFrom::Start(0))?;
                    self.file.read_exact(&mut head)?;
                    self.file.seek(SeekFrom::Start(tail_offset))?;
                    self.file.read_exact(&mut tail)?;
                    hasher.update(b"fixed-edge-samples-v1\0");
                    hasher.update((sample_len as u64).to_le_bytes());
                    hasher.update(head);
                    hasher.update((sample_len as u64).to_le_bytes());
                    hasher.update(tail);
                }
            }
            Ok(())
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

        #[cfg(test)]
        pub(super) fn same_identity(&self, other: &Self) -> bool {
            self.volume == other.volume && self.id == other.id
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

    fn collect_directory_names(buffer: &[u64], names: &mut Vec<OsString>) -> io::Result<()> {
        let byte_len = buffer.len() * size_of::<u64>();
        let name_offset = offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let mut offset = 0usize;
        loop {
            if offset
                .checked_add(name_offset)
                .is_none_or(|end| end > byte_len)
            {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored directory entry exceeded its enumeration buffer",
                ));
            }
            let info = unsafe {
                &*buffer
                    .as_ptr()
                    .cast::<u8>()
                    .add(offset)
                    .cast::<FILE_ID_BOTH_DIR_INFO>()
            };
            let name_bytes = usize::try_from(info.FileNameLength).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored directory entry name length overflowed",
                )
            })?;
            let entry_end = offset
                .checked_add(name_offset)
                .and_then(|start| start.checked_add(name_bytes))
                .filter(|end| *end <= byte_len)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored directory entry name exceeded its enumeration buffer",
                    )
                })?;
            if name_bytes % size_of::<u16>() != 0 || entry_end < offset {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "anchored directory entry name length is invalid",
                ));
            }
            let encoded = unsafe {
                std::slice::from_raw_parts(info.FileName.as_ptr(), name_bytes / size_of::<u16>())
            };
            if encoded != [b'.' as u16] && encoded != [b'.' as u16, b'.' as u16] {
                names.push(OsString::from_wide(encoded));
            }
            let next = info.NextEntryOffset as usize;
            if next == 0 {
                break;
            }
            offset = offset
                .checked_add(next)
                .filter(|next| *next < byte_len)
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored directory entry offset is invalid",
                    )
                })?;
        }
        Ok(())
    }

    fn require_direct_leaf(name: &OsStr) -> io::Result<()> {
        let encoded = name.encode_wide().collect::<Vec<_>>();
        if encoded.is_empty() || encoded.len() > 255 || encoded.contains(&0) {
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
}

#[cfg(all(test, unix))]
mod tests {
    use super::{
        AnchoredRecordDirectory, AnchoredRecordIdentity, AnchoredRecordObservation,
        AnchoredRecordQuarantineError, AnchoredRecordQuarantineReceipt,
        AnchoredRecordRestartDigest, ExactRenameTestStage, RESTART_IDENTITY_EDGE_SAMPLE_BYTES,
        set_exact_rename_test_hook,
    };
    use static_assertions::assert_not_impl_any;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::os::unix::ffi::OsStringExt as _;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    assert_not_impl_any!(
        AnchoredRecordIdentity:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );

    #[test]
    fn exact_noreplace_cfg_matches_the_rustix_target_surface() {
        assert_eq!(
            super::platform::exact_noreplace_rename_supported(),
            cfg!(any(
                target_vendor = "apple",
                target_os = "linux",
                target_os = "android",
                target_os = "redox"
            ))
        );
    }

    #[cfg(not(any(
        target_vendor = "apple",
        target_os = "linux",
        target_os = "android",
        target_os = "redox"
    )))]
    #[test]
    fn exact_quarantine_is_unsupported_before_any_effect() {
        let root = test_root("unsupported-exact-quarantine");
        fs::create_dir_all(&root).expect("create quarantine root");
        let source = root.join("record.json");
        let destination =
            root.join(".record.json.axial-quarantine-88888888888888888888888888888888");
        fs::write(&source, b"rejected restart record").expect("write rejected record");
        let identity = mutation_identity(&root, OsStr::new("record.json"));

        let result = identity.quarantine([0x88; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::Refused(error))
                if error.kind() == std::io::ErrorKind::Unsupported
        ));
        assert_eq!(
            fs::read(&source).expect("read unchanged source"),
            b"rejected restart record"
        );
        assert!(!destination.exists());
        cleanup(&root);
    }
    assert_not_impl_any!(
        AnchoredRecordObservation:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned
    );
    assert_not_impl_any!(
        AnchoredRecordRestartDigest:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );
    assert_not_impl_any!(
        AnchoredRecordQuarantineReceipt:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned,
            AsRef<Path>,
            AsRef<[u8]>
    );

    #[test]
    fn exact_quarantine_preserves_native_name_identity_and_lowercase_suffix() {
        let root = test_root("exact-quarantine");
        fs::create_dir_all(&root).expect("create quarantine root");
        let canonical = OsString::from_vec(b"record-\xff.json".to_vec());
        let source = root.join(&canonical);
        fs::write(&source, b"rejected restart record").expect("write rejected record");
        let identity = mutation_identity(&root, &canonical);
        let suffix = [
            0x00, 0x01, 0x0a, 0x0f, 0x10, 0x2b, 0x3c, 0x4d, 0x5e, 0x6f, 0x70, 0x81, 0x92, 0xa3,
            0xbe, 0xff,
        ];
        let mut destination = OsString::from(".");
        destination.push(&canonical);
        destination.push(".axial-quarantine-00010a0f102b3c4d5e6f708192a3beff");

        let receipt = match identity.quarantine(suffix) {
            Ok(receipt) => receipt,
            Err(_) => panic!("quarantine exact retained record"),
        };

        assert!(!source.exists());
        assert_eq!(
            fs::read(root.join(destination)).expect("read deterministic quarantine destination"),
            b"rejected restart record"
        );
        assert!(receipt.is_current());
        cleanup(&root);
    }

    #[test]
    fn quarantine_collision_is_refused_without_touching_either_leaf() {
        let root = test_root("quarantine-collision");
        fs::create_dir_all(&root).expect("create quarantine root");
        let canonical = OsStr::new("record.json");
        let source = root.join(canonical);
        let destination =
            root.join(".record.json.axial-quarantine-11111111111111111111111111111111");
        fs::write(&source, b"source bytes").expect("write source");
        fs::write(&destination, b"destination bytes").expect("write destination");
        let identity = mutation_identity(&root, canonical);

        let result = identity.quarantine([0x11; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::Refused(_))
        ));
        assert_eq!(fs::read(&source).expect("read source"), b"source bytes");
        assert_eq!(
            fs::read(&destination).expect("read destination"),
            b"destination bytes"
        );
        cleanup(&root);
    }

    #[test]
    fn final_precheck_refuses_source_replacement_without_moving_it() {
        let root = test_root("quarantine-source-replacement");
        fs::create_dir_all(&root).expect("create quarantine root");
        let source = root.join("record.json");
        let original = root.join("original.json");
        fs::write(&source, b"original bytes").expect("write source");
        let identity = mutation_identity(&root, OsStr::new("record.json"));
        let hook_source = source.clone();
        let hook_original = original.clone();
        set_exact_rename_test_hook(ExactRenameTestStage::BeforeFinalPrecheck, move || {
            fs::rename(&hook_source, &hook_original).expect("move original before final precheck");
            fs::write(&hook_source, b"replacement bytes").expect("publish replacement source");
        });

        let result = identity.quarantine([0x22; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::Refused(_))
        ));
        assert_eq!(
            fs::read(&original).expect("read original"),
            b"original bytes"
        );
        assert_eq!(
            fs::read(&source).expect("read replacement"),
            b"replacement bytes"
        );
        assert!(
            !root
                .join(".record.json.axial-quarantine-22222222222222222222222222222222")
                .exists()
        );
        cleanup(&root);
    }

    #[test]
    fn final_precheck_refuses_ancestor_replacement_without_redirecting_effects() {
        let root = test_root("quarantine-ancestor-replacement");
        let held = root.with_extension("held");
        fs::create_dir_all(&root).expect("create quarantine root");
        fs::write(root.join("record.json"), b"held bytes").expect("write source");
        let identity = mutation_identity(&root, OsStr::new("record.json"));
        let hook_root = root.clone();
        let hook_held = held.clone();
        set_exact_rename_test_hook(ExactRenameTestStage::BeforeFinalPrecheck, move || {
            fs::rename(&hook_root, &hook_held).expect("detach held root");
            fs::create_dir_all(&hook_root).expect("create replacement root");
            fs::write(hook_root.join("record.json"), b"replacement bytes")
                .expect("write replacement source");
        });

        let result = identity.quarantine([0x33; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::Refused(_))
        ));
        assert_eq!(
            fs::read(held.join("record.json")).expect("read held source"),
            b"held bytes"
        );
        assert_eq!(
            fs::read(root.join("record.json")).expect("read replacement source"),
            b"replacement bytes"
        );
        cleanup(&root);
        cleanup(&held);
    }

    #[test]
    fn post_rename_destination_substitution_is_applied_unverified() {
        let root = test_root("quarantine-post-rename");
        fs::create_dir_all(&root).expect("create quarantine root");
        let source = root.join("record.json");
        let destination =
            root.join(".record.json.axial-quarantine-44444444444444444444444444444444");
        let retained = root.join("retained-original.json");
        fs::write(&source, b"original bytes").expect("write source");
        let identity = mutation_identity(&root, OsStr::new("record.json"));
        let hook_destination = destination.clone();
        let hook_retained = retained.clone();
        set_exact_rename_test_hook(ExactRenameTestStage::AfterRename, move || {
            fs::rename(&hook_destination, &hook_retained).expect("retain renamed original");
            fs::write(&hook_destination, b"replacement bytes").expect("substitute destination");
        });

        let result = identity.quarantine([0x44; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::AppliedUnverified(_))
        ));
        assert!(!source.exists());
        assert_eq!(
            fs::read(&retained).expect("read retained original"),
            b"original bytes"
        );
        assert_eq!(
            fs::read(&destination).expect("read replacement destination"),
            b"replacement bytes"
        );
        cleanup(&root);
    }

    #[test]
    fn post_sync_destination_drift_is_applied_unverified_without_retry() {
        let root = test_root("quarantine-post-sync");
        fs::create_dir_all(&root).expect("create quarantine root");
        let source = root.join("record.json");
        let destination =
            root.join(".record.json.axial-quarantine-55555555555555555555555555555555");
        let retained = root.join("retained-after-sync.json");
        fs::write(&source, b"original bytes").expect("write source");
        let identity = mutation_identity(&root, OsStr::new("record.json"));
        let hook_destination = destination.clone();
        let hook_retained = retained.clone();
        set_exact_rename_test_hook(ExactRenameTestStage::AfterSync, move || {
            fs::rename(&hook_destination, &hook_retained).expect("move destination after sync");
        });

        let result = identity.quarantine([0x55; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::AppliedUnverified(_))
        ));
        assert!(!source.exists());
        assert!(!destination.exists());
        assert_eq!(
            fs::read(&retained).expect("read single moved record"),
            b"original bytes"
        );
        assert_eq!(
            fs::read_dir(&root)
                .expect("enumerate quarantine root")
                .filter_map(Result::ok)
                .count(),
            1,
            "the consumed authority cannot retry and create a second quarantine"
        );
        cleanup(&root);
    }

    #[test]
    fn overlong_deterministic_destination_is_refused_before_rename() {
        let root = test_root("quarantine-overlong");
        fs::create_dir_all(&root).expect("create quarantine root");
        let canonical = OsString::from("r".repeat(220));
        let source = root.join(&canonical);
        fs::write(&source, b"source bytes").expect("write long-name source");
        let identity = mutation_identity(&root, &canonical);

        let result = identity.quarantine([0x66; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::Refused(_))
        ));
        assert_eq!(fs::read(&source).expect("read source"), b"source bytes");
        assert_eq!(
            fs::read_dir(&root)
                .expect("enumerate quarantine root")
                .filter_map(Result::ok)
                .count(),
            1
        );
        cleanup(&root);
    }

    #[test]
    fn ordinary_restart_identity_is_deterministic_and_mutation_invalidates_admission() {
        let root = test_root("restart-determinism");
        fs::create_dir_all(&root).expect("create anchored root");
        let path = root.join("record.json");
        fs::write(&path, b"same record").expect("write record");
        let first = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("read first record");
        let second = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("read second record");

        let (_, first_digest) = first
            .into_restart_identity()
            .expect("derive first restart identity");
        let (_, second_digest) = second
            .into_restart_identity()
            .expect("derive second restart identity");
        assert_eq!(first_digest.bytes(), second_digest.bytes());

        let stale = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("retain record before mutation");
        fs::write(&path, b"new! record").expect("mutate record in place");
        assert!(stale.into_restart_identity().is_err());
        cleanup(&root);
    }

    #[test]
    fn byte_identical_replacement_cannot_retain_restart_identity() {
        let root = test_root("restart-identical-replacement");
        fs::create_dir_all(&root).expect("create anchored root");
        let path = root.join("record.json");
        fs::write(&path, b"same bytes").expect("write record");
        let (_, original_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read original record")
                .into_restart_identity()
                .expect("derive original restart identity");
        let stale = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("retain original record");

        fs::rename(&path, root.join("old.json")).expect("move original record");
        fs::write(&path, b"same bytes").expect("write byte-identical replacement");

        assert!(stale.into_restart_identity().is_err());
        let (_, replacement_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read byte-identical replacement")
                .into_restart_identity()
                .expect("derive replacement restart identity");
        assert_ne!(original_digest.bytes(), replacement_digest.bytes());
        cleanup(&root);
    }

    #[test]
    fn oversized_restart_identity_uses_deterministic_fixed_edge_samples() {
        const RECORD_LIMIT: usize = 256 * 1024;
        let root = test_root("restart-oversized");
        fs::create_dir_all(&root).expect("create anchored root");
        let path = root.join("record.json");
        let mut bytes = vec![b'm'; RECORD_LIMIT + 1];
        bytes[..RESTART_IDENTITY_EDGE_SAMPLE_BYTES].fill(b'h');
        bytes[RECORD_LIMIT + 1 - RESTART_IDENTITY_EDGE_SAMPLE_BYTES..].fill(b't');
        fs::write(&path, &bytes).expect("write oversized record");

        let first =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), RECORD_LIMIT as u64)
                .expect("read first oversized record");
        let second =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), RECORD_LIMIT as u64)
                .expect("read second oversized record");
        assert_eq!(first.bytes(), None);
        let (_, first_digest) = first
            .into_restart_identity()
            .expect("derive first oversized identity");
        let (_, second_digest) = second
            .into_restart_identity()
            .expect("derive second oversized identity");
        assert_eq!(first_digest.bytes(), second_digest.bytes());

        bytes[0] = b'x';
        fs::write(&path, &bytes).expect("mutate sampled edge");
        let (_, changed_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), RECORD_LIMIT as u64)
                .expect("read changed oversized record")
                .into_restart_identity()
                .expect("derive changed oversized identity");
        assert_ne!(first_digest.bytes(), changed_digest.bytes());
        cleanup(&root);
    }

    #[test]
    fn replacement_directory_changes_restart_identity_for_the_same_leaf_inode() {
        let root = test_root("restart-ancestor");
        let replacement = root.with_extension("replacement");
        let old = root.with_extension("old");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::create_dir_all(&replacement).expect("create replacement root");
        let path = root.join("record.json");
        fs::write(&path, b"same record").expect("write record");
        let (first_identity, first_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read record below original ancestor")
                .into_restart_identity()
                .expect("derive original restart identity");

        fs::rename(&path, replacement.join("record.json"))
            .expect("move same inode below replacement ancestor");
        fs::rename(&root, &old).expect("move original ancestor");
        fs::rename(&replacement, &root).expect("publish replacement ancestor");
        let (second_identity, second_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read same inode below replacement ancestor")
                .into_restart_identity()
                .expect("derive replacement restart identity");

        assert!(first_identity.same_file(&second_identity));
        assert_ne!(first_digest.bytes(), second_digest.bytes());
        cleanup(&root);
        cleanup(&old);
    }

    #[test]
    fn bounded_read_retains_exact_identity_and_rejects_replacement() {
        let root = test_root("bounded-replacement");
        fs::create_dir_all(&root).expect("create anchored root");
        let path = root.join("record.json");
        fs::write(&path, b"{\"valid\":true}").expect("write record");

        let observation = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("read exact record");
        assert_eq!(observation.bytes(), Some(b"{\"valid\":true}".as_slice()));
        assert!(!observation.is_oversized());
        let identity = observation.into_identity();
        assert!(identity.is_current());

        fs::rename(&path, root.join("old.json")).expect("move exact record");
        fs::write(&path, b"replacement").expect("replace exact record");
        assert!(!identity.is_current());
        cleanup(&root);
    }

    #[test]
    fn oversized_record_retains_identity_without_exposing_bytes() {
        let root = test_root("oversized");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::write(root.join("record.json"), [7_u8; 65]).expect("write oversized record");

        let observation = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("inspect oversized record");
        assert!(observation.is_oversized());
        assert_eq!(observation.bytes(), None);
        assert!(observation.into_identity().is_current());
        cleanup(&root);
    }

    #[test]
    fn ancestors_leaves_and_non_regular_records_are_never_followed() {
        let root = test_root("nofollow");
        let outside = test_root("nofollow-outside");
        fs::create_dir_all(root.join("real")).expect("create real parent");
        fs::create_dir_all(&outside).expect("create outside root");
        fs::write(outside.join("record.json"), b"outside").expect("write outside record");
        symlink(&outside, root.join("linked")).expect("link ancestor");
        symlink(outside.join("record.json"), root.join("leaf.json")).expect("link leaf");
        fs::create_dir(root.join("directory.json")).expect("create directory leaf");

        assert!(
            AnchoredRecordObservation::read(&root, Path::new("linked/record.json"), 64).is_err()
        );
        assert!(AnchoredRecordObservation::read(&root, Path::new("leaf.json"), 64).is_err());
        assert!(AnchoredRecordObservation::read(&root, Path::new("directory.json"), 64).is_err());
        assert_eq!(
            fs::read(outside.join("record.json")).expect("outside content retained"),
            b"outside"
        );
        cleanup(&root);
        cleanup(&outside);
    }

    #[test]
    fn hard_link_aliases_cannot_mint_record_identity() {
        let root = test_root("hard-link");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::write(root.join("source.json"), b"source").expect("write source");
        fs::hard_link(root.join("source.json"), root.join("alias.json")).expect("create hard link");

        assert!(AnchoredRecordObservation::read(&root, Path::new("source.json"), 64).is_err());
        assert!(AnchoredRecordObservation::read(&root, Path::new("alias.json"), 64).is_err());
        cleanup(&root);
    }

    #[test]
    fn distinct_exact_records_do_not_share_identity() {
        let root = test_root("distinct");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::write(root.join("first.json"), b"first").expect("write first");
        fs::write(root.join("second.json"), b"second").expect("write second");
        let first = AnchoredRecordObservation::read(&root, Path::new("first.json"), 64)
            .expect("read first")
            .into_identity();
        let second = AnchoredRecordObservation::read(&root, Path::new("second.json"), 64)
            .expect("read second")
            .into_identity();

        assert!(!first.same_file(&second));
        cleanup(&root);
    }

    #[test]
    fn held_directory_rejects_path_replacement_before_leaf_open() {
        let root = test_root("held-directory-replacement");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::write(root.join("record.json"), b"original").expect("write original record");
        let directory = AnchoredRecordDirectory::open(&root).expect("hold anchored directory");

        let old = root.with_extension("old");
        fs::rename(&root, &old).expect("move held directory");
        fs::create_dir_all(&root).expect("create replacement directory");
        fs::write(root.join("record.json"), b"replacement").expect("write replacement record");

        assert!(directory.names().is_err());
        assert!(
            directory
                .read(std::ffi::OsStr::new("record.json"), 64)
                .is_err()
        );
        cleanup(&root);
        cleanup(&old);
    }

    #[test]
    fn canonical_fifo_is_rejected_without_blocking() {
        let root = test_root("fifo");
        fs::create_dir_all(&root).expect("create anchored root");
        let fifo = root.join("record.json");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &fifo,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )
        .expect("create fifo");
        let directory = AnchoredRecordDirectory::open(&root).expect("hold anchored directory");

        let started = Instant::now();
        assert!(
            directory
                .read(std::ffi::OsStr::new("record.json"), 64)
                .is_err()
        );
        assert!(started.elapsed() < Duration::from_secs(1));
        cleanup(&root);
    }

    fn test_root(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "axial-anchored-record-{name}-{}",
            uuid::Uuid::new_v4()
        ))
    }

    fn mutation_identity(root: &Path, name: &OsStr) -> AnchoredRecordIdentity {
        AnchoredRecordDirectory::open(root)
            .expect("hold record directory")
            .read_for_mutation(name, 1024)
            .expect("read mutation-compatible record")
            .into_restart_identity()
            .expect("seal restart identity")
            .0
    }

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::{
        AnchoredRecordDirectory, AnchoredRecordObservation, AnchoredRecordQuarantineError,
        ExactRenameTestStage, set_exact_rename_test_hook,
    };
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

    #[test]
    fn exact_quarantine_is_destination_bound_durable_and_no_replace() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-quarantine-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create anchored root");
        let source = root.join("record.json");
        let destination =
            root.join(".record.json.axial-quarantine-12121212121212121212121212121212");
        fs::write(&source, b"rejected record").expect("write rejected record");
        let identity = AnchoredRecordDirectory::open(&root)
            .expect("hold anchored directory")
            .read_for_mutation(OsStr::new("record.json"), 64)
            .expect("read mutation-compatible record")
            .into_restart_identity()
            .expect("seal restart identity")
            .0;

        let receipt = match identity.quarantine([0x12; 16]) {
            Ok(receipt) => receipt,
            Err(_) => panic!("quarantine exact retained record"),
        };

        assert!(!source.exists());
        assert_eq!(
            fs::read(&destination).expect("read quarantine destination"),
            b"rejected record"
        );
        assert!(receipt.is_current());

        fs::write(&source, b"second source").expect("write second source");
        let second = AnchoredRecordDirectory::open(&root)
            .expect("hold anchored directory")
            .read_for_mutation(OsStr::new("record.json"), 64)
            .expect("read second record")
            .into_restart_identity()
            .expect("seal second identity")
            .0;
        let collision = second.quarantine([0x12; 16]);
        assert!(matches!(
            collision,
            Err(AnchoredRecordQuarantineError::Refused(_))
        ));
        assert_eq!(
            fs::read(&source).expect("read second source"),
            b"second source"
        );
        assert_eq!(
            fs::read(&destination).expect("read first destination"),
            b"rejected record"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn post_sync_destination_drift_is_applied_unverified() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-post-sync-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create anchored root");
        let source = root.join("record.json");
        let destination =
            root.join(".record.json.axial-quarantine-34343434343434343434343434343434");
        let retained = root.join("retained.json");
        fs::write(&source, b"rejected record").expect("write rejected record");
        let identity = AnchoredRecordDirectory::open(&root)
            .expect("hold anchored directory")
            .read_for_mutation(OsStr::new("record.json"), 64)
            .expect("read mutation-compatible record")
            .into_restart_identity()
            .expect("seal restart identity")
            .0;
        set_exact_rename_test_hook(ExactRenameTestStage::AfterSync, move || {
            fs::rename(&destination, &retained).expect("move destination after sync");
        });

        let result = identity.quarantine([0x34; 16]);

        assert!(matches!(
            result,
            Err(AnchoredRecordQuarantineError::AppliedUnverified(_))
        ));
        assert!(!source.exists());
        assert_eq!(
            fs::read(root.join("retained.json")).expect("read retained record"),
            b"rejected record"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn restart_identity_is_deterministic_and_rejects_in_place_mutation() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-restart-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create anchored root");
        let path = root.join("record.json");
        fs::write(&path, b"same record").expect("write record");
        let first = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("read first record");
        let second = AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
            .expect("read second record");
        let (_, first_digest) = first
            .into_restart_identity()
            .expect("derive first restart identity");
        let (_, second_digest) = second
            .into_restart_identity()
            .expect("derive second restart identity");
        assert_eq!(first_digest.bytes(), second_digest.bytes());

        let directory = AnchoredRecordDirectory::open(&root).expect("hold anchored directory");
        let stale = directory
            .read_for_mutation(OsStr::new("record.json"), 64)
            .expect("retain mutation-compatible record");
        fs::write(&path, b"new! record").expect("mutate record in place");
        assert!(stale.into_restart_identity().is_err());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn replacement_directory_changes_restart_identity_for_the_same_leaf_file() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-restart-ancestor-{}",
            uuid::Uuid::new_v4()
        ));
        let replacement = root.with_extension("replacement");
        let old = root.with_extension("old");
        fs::create_dir_all(&root).expect("create anchored root");
        fs::create_dir_all(&replacement).expect("create replacement root");
        let path = root.join("record.json");
        fs::write(&path, b"same record").expect("write record");
        let (first_identity, first_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read original record")
                .into_restart_identity()
                .expect("derive original restart identity");
        drop(first_identity);

        fs::rename(&path, replacement.join("record.json"))
            .expect("move same file below replacement ancestor");
        fs::rename(&root, &old).expect("move original ancestor");
        fs::rename(&replacement, &root).expect("publish replacement ancestor");
        let (_, second_digest) =
            AnchoredRecordObservation::read(&root, Path::new("record.json"), 64)
                .expect("read same file below replacement ancestor")
                .into_restart_identity()
                .expect("derive replacement restart identity");

        assert_ne!(first_digest.bytes(), second_digest.bytes());
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&old);
    }

    #[test]
    fn mutation_compatible_identity_allows_external_exact_rename() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-share-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create anchored root");
        let source = root.join("record.json");
        let destination = root.join("moved.json");
        fs::write(&source, b"{").expect("write record");
        let directory = AnchoredRecordDirectory::open(&root).expect("hold anchored directory");
        let observation = directory
            .read_for_mutation(OsStr::new("record.json"), 64)
            .expect("retain mutation-compatible identity");

        fs::rename(&source, &destination).expect("second delete-capable handle can rename");
        assert!(!observation.into_identity().is_current());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn held_directory_enumerates_names_from_list_capable_handle() {
        let root = std::env::temp_dir().join(format!(
            "axial-anchored-record-windows-list-{}",
            uuid::Uuid::new_v4()
        ));
        fs::create_dir_all(&root).expect("create anchored root");
        fs::write(root.join("record.json"), b"record").expect("write record");
        let directory = AnchoredRecordDirectory::open(&root).expect("hold anchored directory");

        assert!(
            directory
                .names()
                .expect("enumerate held directory")
                .iter()
                .any(|name| Path::new(name).file_name() == Some(OsStr::new("record.json")))
        );
        let _ = fs::remove_dir_all(&root);
    }
}

#[cfg(not(any(unix, windows)))]
mod platform {
    use sha2::Sha256;
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::Path;

    pub(super) struct Directory;
    #[derive(Eq, PartialEq)]
    pub(super) struct DirectoryEpoch;
    pub(super) struct Leaf;
    pub(super) struct RegularFile;
    pub(super) struct ExactRenameReceipt;
    pub(super) struct Temp;

    impl Directory {
        pub(super) fn open(_path: &Path) -> io::Result<Self> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn names(&self) -> io::Result<Vec<OsString>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn names_bounded(
            &self,
            _max_entries: usize,
        ) -> io::Result<Option<Vec<OsString>>> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn epoch(&self) -> io::Result<DirectoryEpoch> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn open_leaf(&self, _name: &OsStr) -> io::Result<Leaf> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }
    }

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

        pub(super) fn name(&self) -> &OsStr {
            OsStr::new("")
        }

        pub(super) fn update_restart_identity(&self, _hasher: &mut Sha256) {}

        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            self.revalidate().map(|()| false)
        }

        pub(super) fn rename_exact(
            &self,
            _file: RegularFile,
            _destination: OsString,
        ) -> Result<ExactRenameReceipt, super::AnchoredRecordQuarantineError> {
            Err(super::AnchoredRecordQuarantineError::Refused(
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
        pub(super) fn size(&self) -> u64 {
            0
        }

        pub(super) fn modified_at_ns(&self) -> io::Result<u64> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn verify_sha1(self, _expected_sha1: &str, _expected_size: u64) -> Option<Self> {
            None
        }

        pub(super) fn read_bounded(self, _max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn digest_bounded(
            self,
            _max_bytes: u64,
        ) -> io::Result<([u8; 32], [u8; 64], Self)> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
            ))
        }

        pub(super) fn update_restart_identity(
            &mut self,
            _hasher: &mut Sha256,
            _full_bytes: Option<&[u8]>,
        ) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "restart-stable record identity is unavailable on this platform",
            ))
        }

        pub(super) fn revalidate(&self) -> bool {
            false
        }

        #[cfg(test)]
        pub(super) fn same_identity(&self, _other: &Self) -> bool {
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
