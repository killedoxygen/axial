//! Identity-bound access to exact regular files below held no-follow directories.

use std::ffi::{OsStr, OsString};
use std::io;
use std::path::Path;

#[path = "registered_artifact.rs"]
pub(crate) mod registered_artifact;

pub(crate) struct AnchoredRecordIdentity {
    leaf: AnchoredLeaf,
    file: AnchoredRegularFile,
}

pub(crate) struct AnchoredRecordDirectory(platform::Directory);

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

    pub(crate) fn into_identity(self) -> AnchoredRecordIdentity {
        match self {
            Self::Bytes { identity, .. } | Self::Oversized { identity } => identity,
        }
    }
}

impl AnchoredRecordDirectory {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        platform::Directory::open(path).map(Self)
    }

    pub(crate) fn names(&self) -> io::Result<Vec<OsString>> {
        self.0.names()
    }

    pub(crate) fn read(
        &self,
        name: &OsStr,
        max_bytes: u64,
    ) -> io::Result<AnchoredRecordObservation> {
        let leaf = self.0.open_leaf(name).map(AnchoredLeaf)?;
        AnchoredRecordObservation::read_leaf(leaf, max_bytes, false)
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

    #[cfg(test)]
    pub(crate) fn is_current(&self) -> bool {
        self.revalidate().is_ok()
    }

    #[cfg(test)]
    pub(crate) fn same_file(&self, other: &Self) -> bool {
        self.file.same_identity(&other.file)
    }
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
        self.0.quarantine_existing()
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

    fn verify_sha1(self, expected_sha1: &str, expected_size: u64) -> Option<Self> {
        self.0.verify_sha1(expected_sha1, expected_size).map(Self)
    }

    fn read_bounded(self, max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
        self.0
            .read_bounded(max_bytes)
            .map(|(bytes, file)| (bytes, Self(file)))
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
    use rustix::fs::{AtFlags, FileType, Mode, OFlags, RenameFlags};
    use sha1::{Digest as _, Sha1};
    use std::ffi::{OsStr, OsString};
    use std::io::{self, Read as _, Seek as _, SeekFrom};
    use std::os::unix::ffi::OsStringExt as _;
    use std::path::{Component, Path, PathBuf};
    use std::sync::Arc;

    #[derive(Clone)]
    struct HeldDirectory {
        handle: Arc<OwnedFd>,
        parent: Option<Arc<OwnedFd>>,
        name: Option<OsString>,
        device: u64,
        inode: u64,
    }

    pub(super) struct Directory {
        root_path: PathBuf,
        directories: Vec<HeldDirectory>,
        parent: Arc<OwnedFd>,
    }

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
        device: u64,
        inode: u64,
        size: u64,
        modified_seconds: i64,
        modified_nanoseconds: u64,
        changed_seconds: i64,
        changed_nanoseconds: u64,
    }

    pub(super) struct Temp {
        name: OsString,
        writer: Option<std::fs::File>,
        control: std::fs::File,
        device: u64,
        inode: u64,
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
            self.revalidate()?;
            let entries = rustix::fs::Dir::read_from(self.parent.as_ref())?;
            let mut names = Vec::new();
            for entry in entries {
                let entry = entry?;
                let name = entry.file_name().to_bytes();
                if name != b"." && name != b".." {
                    names.push(OsString::from_vec(name.to_vec()));
                }
            }
            self.revalidate()?;
            Ok(names)
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
                    device: stat.st_dev,
                    inode: stat.st_ino,
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

        pub(super) fn quarantine_existing(&self) -> io::Result<()> {
            self.revalidate()?;
            let file = rustix::fs::openat(
                self.parent.as_ref(),
                &self.leaf,
                OFlags::RDONLY | OFlags::NONBLOCK | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(io::Error::from)?;
            let source = rustix::fs::fstat(&file).map_err(io::Error::from)?;
            if FileType::from_raw_mode(source.st_mode) != FileType::RegularFile
                || source.st_nlink != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is not an exact regular file",
                ));
            }
            let quarantine = OsString::from(format!(
                ".axial-quarantine-{}",
                uuid::Uuid::new_v4().simple()
            ));
            rustix::fs::renameat_with(
                self.parent.as_ref(),
                &self.leaf,
                self.parent.as_ref(),
                &quarantine,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
            rustix::fs::fsync(self.parent.as_ref()).map_err(io::Error::from)?;
            self.revalidate()?;
            if !self.target_is_missing()? {
                return Err(identity_changed(
                    "anchored record quarantine did not vacate leaf",
                ));
            }
            let quarantined =
                rustix::fs::statat(self.parent.as_ref(), &quarantine, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            if quarantined.st_dev != source.st_dev || quarantined.st_ino != source.st_ino {
                return Err(identity_changed(
                    "anchored record quarantine identity changed",
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
                device: stat.st_dev,
                inode: stat.st_ino,
            })
        }

        pub(super) fn remove_temp(&self, temp: Temp) {
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW);
            if current
                .is_ok_and(|current| current.st_dev == temp.device && current.st_ino == temp.inode)
            {
                let _ = rustix::fs::unlinkat(self.parent.as_ref(), &temp.name, AtFlags::empty());
            }
            let _ = rustix::fs::fsync(self.parent.as_ref());
        }

        pub(super) fn promote_temp(&self, temp: &Temp) -> io::Result<()> {
            let held = rustix::fs::fstat(&temp.control).map_err(io::Error::from)?;
            let current =
                rustix::fs::statat(self.parent.as_ref(), &temp.name, AtFlags::SYMLINK_NOFOLLOW)
                    .map_err(io::Error::from)?;
            if held.st_dev != temp.device
                || held.st_ino != temp.inode
                || current.st_dev != temp.device
                || current.st_ino != temp.inode
                || FileType::from_raw_mode(current.st_mode) != FileType::RegularFile
            {
                return Err(identity_changed("anchored record temp changed"));
            }
            rustix::fs::renameat_with(
                self.parent.as_ref(),
                &temp.name,
                self.parent.as_ref(),
                &self.leaf,
                RenameFlags::NOREPLACE,
            )
            .map_err(io::Error::from)?;
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
            let file = RegularFile {
                parent: self.parent.clone(),
                leaf: self.leaf.clone(),
                file: std::fs::File::from(handle),
                device: stat.st_dev,
                inode: stat.st_ino,
                size: u64::try_from(stat.st_size).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidData,
                        "anchored record size is invalid",
                    )
                })?,
                modified_seconds: stat.st_mtime,
                modified_nanoseconds: stat.st_mtime_nsec,
                changed_seconds: stat.st_ctime,
                changed_nanoseconds: stat.st_ctime_nsec,
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
        }

        pub(super) fn verify_sha1(
            mut self,
            expected_sha1: &str,
            expected_size: u64,
        ) -> Option<Self> {
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
            if self.size > max_bytes || !self.revalidate() {
                return Err(identity_changed(
                    "anchored record changed or exceeds its read bound",
                ));
            }
            self.file.seek(SeekFrom::Start(0))?;
            let capacity = usize::try_from(self.size).map_err(|_| {
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
            if bytes.len() as u64 != self.size || !self.revalidate() {
                return Err(identity_changed("anchored record changed during reading"));
            }
            Ok((bytes, self))
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

        #[cfg(test)]
        pub(super) fn same_identity(&self, other: &Self) -> bool {
            self.device == other.device && self.inode == other.inode
        }

        fn matches(&self, stat: rustix::fs::Stat) -> bool {
            FileType::from_raw_mode(stat.st_mode) == FileType::RegularFile
                && stat.st_nlink == 1
                && stat.st_dev == self.device
                && stat.st_ino == self.inode
                && u64::try_from(stat.st_size).ok() == Some(self.size)
                && stat.st_mtime == self.modified_seconds
                && stat.st_mtime_nsec == self.modified_nanoseconds
                && stat.st_ctime == self.changed_seconds
                && stat.st_ctime_nsec == self.changed_nanoseconds
        }
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
            device: stat.st_dev,
            inode: stat.st_ino,
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
                device: stat.st_dev,
                inode: stat.st_ino,
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
        if root_stat.st_dev != expected_root.device || root_stat.st_ino != expected_root.inode {
            return Err(identity_changed("anchored record root changed"));
        }
        let held_root =
            rustix::fs::fstat(expected_root.handle.as_ref()).map_err(io::Error::from)?;
        if held_root.st_dev != expected_root.device || held_root.st_ino != expected_root.inode {
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
                || stat.st_dev != directory.device
                || stat.st_ino != directory.inode
            {
                return Err(identity_changed("anchored record ancestor changed"));
            }
            let held = rustix::fs::fstat(directory.handle.as_ref()).map_err(io::Error::from)?;
            if held.st_dev != directory.device || held.st_ino != directory.inode {
                return Err(identity_changed("anchored record held ancestor changed"));
            }
        }
        Ok(())
    }

    fn require_direct_leaf(name: &OsStr) -> io::Result<()> {
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
}

#[cfg(windows)]
mod platform {
    use sha1::{Digest as _, Sha1};
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
            }
            self.revalidate()?;
            Ok(names)
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

        pub(super) fn quarantine_existing(&self) -> io::Result<()> {
            self.revalidate()?;
            let file = open_relative(
                &self.parent,
                &self.leaf,
                None,
                DELETE | FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let basic = query::<FILE_BASIC_INFO>(&file, FileBasicInfo)?;
            let standard = query::<FILE_STANDARD_INFO>(&file, FileStandardInfo)?;
            if basic.FileAttributes & (FILE_ATTRIBUTE_DIRECTORY | FILE_ATTRIBUTE_REPARSE_POINT) != 0
                || standard.Directory
                || standard.NumberOfLinks != 1
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "anchored record leaf is not an exact regular file",
                ));
            }
            let source_id = query::<FILE_ID_INFO>(&file, FileIdInfo)?;
            let quarantine = OsString::from(format!(
                ".axial-quarantine-{}",
                uuid::Uuid::new_v4().simple()
            ));
            rename_relative(&file, &self.parent, &quarantine)?;
            self.revalidate()?;
            if !self.target_is_missing()? {
                return Err(identity_changed(
                    "anchored record quarantine did not vacate leaf",
                ));
            }
            let quarantined = open_relative(
                &self.parent,
                &quarantine,
                None,
                FILE_READ_ATTRIBUTES,
                FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
                FILE_OPEN,
            )?;
            let quarantined = query::<FILE_ID_INFO>(&quarantined, FileIdInfo)?;
            if quarantined.VolumeSerialNumber != source_id.VolumeSerialNumber
                || quarantined.FileId.Identifier != source_id.FileId.Identifier
            {
                return Err(identity_changed(
                    "anchored record quarantine identity changed",
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
            let file = match open_relative(
                &self.parent,
                &self.leaf,
                Some(false),
                GENERIC_READ | FILE_READ_ATTRIBUTES,
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
    use super::{AnchoredRecordDirectory, AnchoredRecordIdentity, AnchoredRecordObservation};
    use static_assertions::assert_not_impl_any;
    use std::fs;
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
    assert_not_impl_any!(
        AnchoredRecordObservation:
            Clone,
            std::fmt::Debug,
            serde::Serialize,
            serde::de::DeserializeOwned
    );

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

    fn cleanup(path: &Path) {
        let _ = fs::remove_dir_all(path);
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use super::AnchoredRecordDirectory;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::Path;

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
    use std::ffi::{OsStr, OsString};
    use std::io;
    use std::path::Path;

    pub(super) struct Directory;
    pub(super) struct Leaf;
    pub(super) struct RegularFile;
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

        pub(super) fn target_is_missing(&self) -> io::Result<bool> {
            self.revalidate().map(|()| false)
        }

        pub(super) fn quarantine_existing(&self) -> io::Result<()> {
            self.revalidate()
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

        pub(super) fn verify_sha1(self, _expected_sha1: &str, _expected_size: u64) -> Option<Self> {
            None
        }

        pub(super) fn read_bounded(self, _max_bytes: u64) -> io::Result<(Vec<u8>, Self)> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "anchored records are unavailable on this platform",
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
}
