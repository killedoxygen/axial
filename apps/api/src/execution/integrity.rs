//! Bounded integrity verification of exact live launcher-owned inventory authority.

use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::{
    AppState, InstanceLifecycleLease, KnownGoodVerificationLease, KnownGoodVerificationUnavailable,
};
#[cfg(test)]
use axial_minecraft::known_good::KnownGoodArtifactKind;
use axial_minecraft::known_good::{
    KnownGoodEntry, KnownGoodIntegrity, KnownGoodPhysicalPath, KnownGoodRoot,
    LaunchTier0RuntimeSelection, LaunchTier1AdmittedFile, MAX_LAUNCH_TIER1_AGGREGATE_BYTES,
    known_good_entry_path, known_good_link_target_matches,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

const MAX_INTEGRITY_TIER0_FACTS: usize = 64;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MetadataKind {
    File,
    Directory,
    Link,
    #[cfg(unix)]
    Other,
}

#[derive(Clone, Copy, Debug)]
struct MetadataObservation {
    kind: MetadataKind,
    size: u64,
    modified: Option<SystemTime>,
}

enum ContentHashObservation {
    Hashed { digest: String },
    SizeDrift { observed_size: u64 },
    WrongType,
    ChangedDuringRead,
    BudgetRefused,
}

struct ContentHashResult {
    observation: io::Result<ContentHashObservation>,
    bytes_read: u64,
}

fn read_exact_sha1(
    reader: &mut impl Read,
    expected_size: u64,
    byte_budget: u64,
) -> ContentHashResult {
    if expected_size > byte_budget {
        return ContentHashResult {
            observation: Ok(ContentHashObservation::BudgetRefused),
            bytes_read: 0,
        };
    }

    let mut hasher = Sha1::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes_read = 0_u64;
    while bytes_read < expected_size {
        let remaining = expected_size - bytes_read;
        let limit = remaining.min(buffer.len() as u64) as usize;
        let count = match reader.read(&mut buffer[..limit]) {
            Ok(count) => count,
            Err(error) => {
                return ContentHashResult {
                    observation: Err(error),
                    bytes_read,
                };
            }
        };
        if count == 0 {
            return ContentHashResult {
                observation: Ok(ContentHashObservation::SizeDrift {
                    observed_size: bytes_read,
                }),
                bytes_read,
            };
        }
        bytes_read += count as u64;
        hasher.update(&buffer[..count]);
    }
    ContentHashResult {
        observation: Ok(ContentHashObservation::Hashed {
            digest: format!("{:x}", hasher.finalize()),
        }),
        bytes_read,
    }
}

trait MetadataReader {
    fn symlink_metadata(&self, path: &KnownGoodPhysicalPath) -> io::Result<MetadataObservation>;
    fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf>;
    fn revalidate(&self) -> io::Result<()> {
        Ok(())
    }
}

trait ContentReader {
    fn hash_file(
        &self,
        path: &KnownGoodPhysicalPath,
        expected_size: u64,
        byte_budget: u64,
    ) -> ContentHashResult;

    fn revalidate(&self) -> io::Result<()>;
}

#[cfg(unix)]
mod confined_fs {
    use super::{
        ContentHashObservation, ContentHashResult, MetadataKind, MetadataObservation,
        read_exact_sha1,
    };
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::ffi::OsString;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStringExt;
    use std::path::{Component, Path, PathBuf};
    use std::rc::Rc;
    use std::time::{Duration, SystemTime};

    #[derive(Default)]
    pub(super) struct Reader {
        directories: RefCell<HashMap<PathBuf, Rc<OwnedFd>>>,
        blocked: RefCell<HashMap<PathBuf, io::ErrorKind>>,
        roots: RefCell<HashSet<PathBuf>>,
        leaves: RefCell<Vec<HeldLeaf>>,
    }

    struct HeldLeaf {
        parent: Rc<OwnedFd>,
        name: OsString,
        file: Rc<std::fs::File>,
        device: u64,
        inode: u64,
        size: i64,
        modified_seconds: i64,
        modified_nanoseconds: u64,
        changed_seconds: i64,
        changed_nanoseconds: u64,
    }

    impl Reader {
        fn root(&self, root: &Path) -> io::Result<Rc<OwnedFd>> {
            if let Some(kind) = self.blocked.borrow().get(root).copied() {
                return Err(io::Error::from(kind));
            }
            if let Some(handle) = self.directories.borrow().get(root).cloned() {
                return Ok(handle);
            }
            let handle = match rustix::fs::open(
                root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map(Rc::new)
            .map_err(io::Error::from)
            {
                Ok(handle) => handle,
                Err(error) => {
                    self.blocked
                        .borrow_mut()
                        .insert(root.to_path_buf(), error.kind());
                    return Err(error);
                }
            };
            self.directories
                .borrow_mut()
                .insert(root.to_path_buf(), handle.clone());
            self.roots.borrow_mut().insert(root.to_path_buf());
            Ok(handle)
        }

        fn parent(&self, path: &KnownGoodPhysicalPath) -> io::Result<(Rc<OwnedFd>, OsString)> {
            let mut handle = self.root(path.root())?;
            let mut absolute = path.root().to_path_buf();
            let mut components = path.relative().components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good path escaped its physical root",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                absolute.push(name);
                if let Some(kind) = self.blocked.borrow().get(&absolute).copied() {
                    return Err(io::Error::from(kind));
                }
                if let Some(cached) = self.directories.borrow().get(&absolute).cloned() {
                    handle = cached;
                    continue;
                }
                let child = match rustix::fs::openat(
                    handle.as_ref(),
                    name,
                    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map(Rc::new)
                .map_err(io::Error::from)
                {
                    Ok(child) => child,
                    Err(error) => {
                        self.blocked
                            .borrow_mut()
                            .insert(absolute.clone(), error.kind());
                        return Err(error);
                    }
                };
                self.directories
                    .borrow_mut()
                    .insert(absolute.clone(), child.clone());
                handle = child;
            }
            leaf.map(|leaf| (handle, leaf)).ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "known-good leaf is empty")
            })
        }

        pub(super) fn metadata(
            &self,
            path: &KnownGoodPhysicalPath,
        ) -> io::Result<MetadataObservation> {
            let (parent, leaf) = self.parent(path)?;
            let stat = rustix::fs::statat(parent.as_ref(), &leaf, AtFlags::SYMLINK_NOFOLLOW)
                .map_err(io::Error::from)?;
            let kind = match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => MetadataKind::File,
                FileType::Directory => MetadataKind::Directory,
                FileType::Symlink => MetadataKind::Link,
                _ => MetadataKind::Other,
            };
            Ok(MetadataObservation {
                kind,
                size: stat.st_size.try_into().unwrap_or_default(),
                modified: (stat.st_mtime >= 0)
                    .then(|| SystemTime::UNIX_EPOCH + Duration::from_secs(stat.st_mtime as u64)),
            })
        }

        pub(super) fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf> {
            let (parent, leaf) = self.parent(path)?;
            let target = rustix::fs::readlinkat(parent.as_ref(), &leaf, Vec::new())
                .map_err(io::Error::from)?;
            Ok(PathBuf::from(OsString::from_vec(target.into_bytes())))
        }

        pub(super) fn hash_file(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            let mut bytes_read = 0_u64;
            let observation = (|| -> io::Result<ContentHashObservation> {
                if expected_size > byte_budget {
                    return Ok(ContentHashObservation::BudgetRefused);
                }
                let (parent, leaf) = self.parent(path)?;
                let handle = rustix::fs::openat(
                    parent.as_ref(),
                    &leaf,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?;
                let before = rustix::fs::fstat(&handle).map_err(io::Error::from)?;
                if FileType::from_raw_mode(before.st_mode) != FileType::RegularFile {
                    return Ok(ContentHashObservation::WrongType);
                }
                let before_size = before.st_size.try_into().unwrap_or_default();
                let file = Rc::new(std::fs::File::from(handle));
                self.leaves.borrow_mut().push(HeldLeaf {
                    parent,
                    name: leaf,
                    file: file.clone(),
                    device: before.st_dev,
                    inode: before.st_ino,
                    size: before.st_size,
                    modified_seconds: before.st_mtime,
                    modified_nanoseconds: before.st_mtime_nsec,
                    changed_seconds: before.st_ctime,
                    changed_nanoseconds: before.st_ctime_nsec,
                });
                if before_size != expected_size {
                    return Ok(ContentHashObservation::SizeDrift {
                        observed_size: before_size,
                    });
                }

                let mut readable = file.as_ref();
                let result = read_exact_sha1(&mut readable, expected_size, byte_budget);
                bytes_read = result.bytes_read;
                let digest = match result.observation? {
                    ContentHashObservation::Hashed { digest } => digest,
                    observation => return Ok(observation),
                };
                let after = rustix::fs::fstat(file.as_ref()).map_err(io::Error::from)?;
                let after_size = after.st_size.try_into().unwrap_or_default();
                if after_size != expected_size {
                    return Ok(ContentHashObservation::SizeDrift {
                        observed_size: after_size,
                    });
                }
                if before.st_dev != after.st_dev
                    || before.st_ino != after.st_ino
                    || before.st_mtime != after.st_mtime
                    || before.st_mtime_nsec != after.st_mtime_nsec
                    || before.st_ctime != after.st_ctime
                    || before.st_ctime_nsec != after.st_ctime_nsec
                {
                    return Ok(ContentHashObservation::ChangedDuringRead);
                }
                Ok(ContentHashObservation::Hashed { digest })
            })();
            ContentHashResult {
                observation,
                bytes_read,
            }
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            let directories = self.directories.borrow();
            let roots = self.roots.borrow();
            for (path, held) in directories.iter() {
                let held_stat = rustix::fs::fstat(held.as_ref()).map_err(io::Error::from)?;
                let current_stat = if roots.contains(path) {
                    let current = rustix::fs::open(
                        path,
                        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                        Mode::empty(),
                    )
                    .map_err(io::Error::from)?;
                    rustix::fs::fstat(&current).map_err(io::Error::from)?
                } else {
                    let parent_path = path.parent().ok_or_else(|| {
                        io::Error::new(io::ErrorKind::PermissionDenied, "missing held parent")
                    })?;
                    let parent = directories.get(parent_path).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::PermissionDenied, "unheld parent")
                    })?;
                    rustix::fs::statat(
                        parent.as_ref(),
                        path.file_name().ok_or_else(|| {
                            io::Error::new(io::ErrorKind::PermissionDenied, "missing child name")
                        })?,
                        AtFlags::SYMLINK_NOFOLLOW,
                    )
                    .map_err(io::Error::from)?
                };
                if held_stat.st_dev != current_stat.st_dev
                    || held_stat.st_ino != current_stat.st_ino
                    || FileType::from_raw_mode(current_stat.st_mode) != FileType::Directory
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good ancestor identity changed",
                    ));
                }
            }
            for leaf in self.leaves.borrow().iter() {
                let held_stat = rustix::fs::fstat(leaf.file.as_ref()).map_err(io::Error::from)?;
                if FileType::from_raw_mode(held_stat.st_mode) != FileType::RegularFile
                    || held_stat.st_dev != leaf.device
                    || held_stat.st_ino != leaf.inode
                    || held_stat.st_size != leaf.size
                    || held_stat.st_mtime != leaf.modified_seconds
                    || held_stat.st_mtime_nsec != leaf.modified_nanoseconds
                    || held_stat.st_ctime != leaf.changed_seconds
                    || held_stat.st_ctime_nsec != leaf.changed_nanoseconds
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good leaf changed after content read",
                    ));
                }
                let current = rustix::fs::openat(
                    leaf.parent.as_ref(),
                    &leaf.name,
                    OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                    Mode::empty(),
                )
                .map_err(io::Error::from)?;
                let current_stat = rustix::fs::fstat(&current).map_err(io::Error::from)?;
                if FileType::from_raw_mode(current_stat.st_mode) != FileType::RegularFile
                    || current_stat.st_dev != leaf.device
                    || current_stat.st_ino != leaf.inode
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good leaf identity changed",
                    ));
                }
            }
            Ok(())
        }
    }
}

#[cfg(windows)]
mod confined_fs {
    use super::{
        ContentHashObservation, ContentHashResult, MetadataKind, MetadataObservation,
        read_exact_sha1,
    };
    use axial_minecraft::known_good::KnownGoodPhysicalPath;
    use std::cell::RefCell;
    use std::collections::{HashMap, HashSet};
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::{AsRawHandle, FromRawHandle};
    use std::path::{Component, Path, PathBuf};
    use std::ptr;
    use std::rc::Rc;
    use std::time::SystemTime;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT,
        FILE_SYNCHRONOUS_IO_NONALERT, NtCreateFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
        UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo,
        FileIdInfo, FileStandardInfo, GetFileInformationByHandleEx, SYNCHRONIZE,
    };
    use windows_sys::Win32::System::IO::IO_STATUS_BLOCK;

    #[derive(Default)]
    pub(super) struct Reader {
        directories: RefCell<HashMap<PathBuf, Rc<fs::File>>>,
        blocked: RefCell<HashMap<PathBuf, io::ErrorKind>>,
        roots: RefCell<HashSet<PathBuf>>,
        leaves: RefCell<Vec<HeldLeaf>>,
    }

    struct HeldLeaf {
        parent: Rc<fs::File>,
        name: OsString,
        file: Rc<fs::File>,
        volume_serial_number: u64,
        file_id: [u8; 16],
        size: i64,
        modified: i64,
        changed: i64,
    }

    impl Reader {
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

        fn root(&self, root: &Path) -> io::Result<Rc<fs::File>> {
            if let Some(kind) = self.blocked.borrow().get(root).copied() {
                return Err(io::Error::from(kind));
            }
            if let Some(handle) = self.directories.borrow().get(root).cloned() {
                return Ok(handle);
            }
            let file = Self::open_root_exact(root)?;
            let file = Rc::new(file);
            self.directories
                .borrow_mut()
                .insert(root.to_path_buf(), file.clone());
            self.roots.borrow_mut().insert(root.to_path_buf());
            Ok(file)
        }

        fn open_root_exact(root: &Path) -> io::Result<fs::File> {
            let mut options = fs::OpenOptions::new();
            options
                .access_mode(FILE_READ_ATTRIBUTES)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
                .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT | FILE_FLAG_BACKUP_SEMANTICS);
            let file = options.open(root)?;
            Self::require_exact_directory(&file)?;
            Ok(file)
        }

        fn require_exact_directory(file: &fs::File) -> io::Result<()> {
            let basic: FILE_BASIC_INFO = Self::query(file, FileBasicInfo)?;
            let standard: FILE_STANDARD_INFO = Self::query(file, FileStandardInfo)?;
            if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
                || !standard.Directory
            {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "known-good ancestor is not an exact directory",
                ));
            }
            Ok(())
        }

        fn open_relative(
            parent: &fs::File,
            name: &OsStr,
            directory: Option<bool>,
        ) -> io::Result<fs::File> {
            Self::open_relative_with_access(parent, name, directory, FILE_READ_ATTRIBUTES)
        }

        fn open_relative_with_access(
            parent: &fs::File,
            name: &OsStr,
            directory: Option<bool>,
            access: u32,
        ) -> io::Result<fs::File> {
            let mut encoded = name.encode_wide().collect::<Vec<_>>();
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
                    FILE_SHARE_READ | FILE_SHARE_WRITE,
                    FILE_OPEN,
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

        fn parent(&self, path: &KnownGoodPhysicalPath) -> io::Result<(Rc<fs::File>, OsString)> {
            let mut handle = self.root(path.root())?;
            let mut absolute = path.root().to_path_buf();
            let mut components = path.relative().components().peekable();
            let mut leaf = None;
            while let Some(component) = components.next() {
                let Component::Normal(name) = component else {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "unsafe path",
                    ));
                };
                if components.peek().is_none() {
                    leaf = Some(name.to_os_string());
                    break;
                }
                absolute.push(name);
                if let Some(kind) = self.blocked.borrow().get(&absolute).copied() {
                    return Err(io::Error::from(kind));
                }
                if let Some(cached) = self.directories.borrow().get(&absolute).cloned() {
                    handle = cached;
                    continue;
                }
                let child =
                    match Self::open_relative(handle.as_ref(), name, Some(true)).and_then(|file| {
                        Self::require_exact_directory(&file)?;
                        Ok(Rc::new(file))
                    }) {
                        Ok(child) => child,
                        Err(error) => {
                            self.blocked
                                .borrow_mut()
                                .insert(absolute.clone(), error.kind());
                            return Err(error);
                        }
                    };
                self.directories
                    .borrow_mut()
                    .insert(absolute.clone(), child.clone());
                handle = child;
            }
            leaf.map(|leaf| (handle, leaf)).ok_or_else(|| {
                io::Error::new(io::ErrorKind::PermissionDenied, "known-good leaf is empty")
            })
        }

        pub(super) fn metadata(
            &self,
            path: &KnownGoodPhysicalPath,
        ) -> io::Result<MetadataObservation> {
            let (parent, leaf) = self.parent(path)?;
            let file = Self::open_relative(parent.as_ref(), &leaf, None)?;
            let basic: FILE_BASIC_INFO = Self::query(&file, FileBasicInfo)?;
            let standard: FILE_STANDARD_INFO = Self::query(&file, FileStandardInfo)?;
            let kind = if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                MetadataKind::Link
            } else if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || standard.Directory {
                MetadataKind::Directory
            } else {
                MetadataKind::File
            };
            Ok(MetadataObservation {
                kind,
                size: standard.EndOfFile.try_into().unwrap_or_default(),
                modified: (basic.LastWriteTime != 0).then_some(SystemTime::UNIX_EPOCH),
            })
        }

        pub(super) fn read_link(&self, _path: &KnownGoodPhysicalPath) -> io::Result<PathBuf> {
            Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "Windows runtime links are not admitted to launch Tier 0",
            ))
        }

        pub(super) fn hash_file(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            let mut bytes_read = 0_u64;
            let observation = (|| -> io::Result<ContentHashObservation> {
                if expected_size > byte_budget {
                    return Ok(ContentHashObservation::BudgetRefused);
                }
                let (parent, leaf) = self.parent(path)?;
                let file = Rc::new(Self::open_relative_with_access(
                    parent.as_ref(),
                    &leaf,
                    Some(false),
                    GENERIC_READ,
                )?);
                let before_basic: FILE_BASIC_INFO = Self::query(file.as_ref(), FileBasicInfo)?;
                let before_standard: FILE_STANDARD_INFO =
                    Self::query(file.as_ref(), FileStandardInfo)?;
                if before_basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || before_basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    || before_standard.Directory
                {
                    return Ok(ContentHashObservation::WrongType);
                }
                let before_size = before_standard.EndOfFile.try_into().unwrap_or_default();
                let before_id: FILE_ID_INFO = Self::query(file.as_ref(), FileIdInfo)?;
                self.leaves.borrow_mut().push(HeldLeaf {
                    parent,
                    name: leaf,
                    file: file.clone(),
                    volume_serial_number: before_id.VolumeSerialNumber,
                    file_id: before_id.FileId.Identifier,
                    size: before_standard.EndOfFile,
                    modified: before_basic.LastWriteTime,
                    changed: before_basic.ChangeTime,
                });
                if before_size != expected_size {
                    return Ok(ContentHashObservation::SizeDrift {
                        observed_size: before_size,
                    });
                }

                let mut readable = file.as_ref();
                let result = read_exact_sha1(&mut readable, expected_size, byte_budget);
                bytes_read = result.bytes_read;
                let digest = match result.observation? {
                    ContentHashObservation::Hashed { digest } => digest,
                    observation => return Ok(observation),
                };
                let after_basic: FILE_BASIC_INFO = Self::query(file.as_ref(), FileBasicInfo)?;
                let after_standard: FILE_STANDARD_INFO =
                    Self::query(file.as_ref(), FileStandardInfo)?;
                let after_id: FILE_ID_INFO = Self::query(file.as_ref(), FileIdInfo)?;
                let after_size = after_standard.EndOfFile.try_into().unwrap_or_default();
                if after_size != expected_size {
                    return Ok(ContentHashObservation::SizeDrift {
                        observed_size: after_size,
                    });
                }
                if before_id.VolumeSerialNumber != after_id.VolumeSerialNumber
                    || before_id.FileId.Identifier != after_id.FileId.Identifier
                    || before_basic.LastWriteTime != after_basic.LastWriteTime
                    || before_basic.ChangeTime != after_basic.ChangeTime
                {
                    return Ok(ContentHashObservation::ChangedDuringRead);
                }
                Ok(ContentHashObservation::Hashed { digest })
            })();
            ContentHashResult {
                observation,
                bytes_read,
            }
        }

        pub(super) fn revalidate(&self) -> io::Result<()> {
            let directories = self.directories.borrow();
            for root in self.roots.borrow().iter() {
                let held = directories.get(root).ok_or_else(|| {
                    io::Error::new(io::ErrorKind::PermissionDenied, "missing held root")
                })?;
                let current = Self::open_root_exact(root)?;
                let held_id: FILE_ID_INFO = Self::query(held, FileIdInfo)?;
                let current_id: FILE_ID_INFO = Self::query(&current, FileIdInfo)?;
                if held_id.VolumeSerialNumber != current_id.VolumeSerialNumber
                    || held_id.FileId.Identifier != current_id.FileId.Identifier
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good root identity changed",
                    ));
                }
            }
            for leaf in self.leaves.borrow().iter() {
                let held_basic: FILE_BASIC_INFO = Self::query(leaf.file.as_ref(), FileBasicInfo)?;
                let held_standard: FILE_STANDARD_INFO =
                    Self::query(leaf.file.as_ref(), FileStandardInfo)?;
                let held_id: FILE_ID_INFO = Self::query(leaf.file.as_ref(), FileIdInfo)?;
                if held_basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || held_basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    || held_standard.Directory
                    || held_id.VolumeSerialNumber != leaf.volume_serial_number
                    || held_id.FileId.Identifier != leaf.file_id
                    || held_standard.EndOfFile != leaf.size
                    || held_basic.LastWriteTime != leaf.modified
                    || held_basic.ChangeTime != leaf.changed
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good leaf changed after content read",
                    ));
                }
                let current = Self::open_relative(leaf.parent.as_ref(), &leaf.name, Some(false))?;
                let current_basic: FILE_BASIC_INFO = Self::query(&current, FileBasicInfo)?;
                let current_standard: FILE_STANDARD_INFO = Self::query(&current, FileStandardInfo)?;
                let current_id: FILE_ID_INFO = Self::query(&current, FileIdInfo)?;
                if current_basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
                    || current_basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0
                    || current_standard.Directory
                    || current_id.VolumeSerialNumber != leaf.volume_serial_number
                    || current_id.FileId.Identifier != leaf.file_id
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good leaf identity changed",
                    ));
                }
            }
            Ok(())
        }
    }
}

#[derive(Default)]
struct FilesystemMetadataReader {
    #[cfg(any(unix, windows))]
    inner: confined_fs::Reader,
}

impl MetadataReader for FilesystemMetadataReader {
    fn symlink_metadata(&self, path: &KnownGoodPhysicalPath) -> io::Result<MetadataObservation> {
        #[cfg(any(unix, windows))]
        return self.inner.metadata(path);
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "race-resistant known-good metadata is unavailable on this platform",
        ))
    }

    fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf> {
        #[cfg(any(unix, windows))]
        return self.inner.read_link(path);
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "race-resistant known-good link inspection is unavailable on this platform",
        ))
    }

    fn revalidate(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        return self.inner.revalidate();
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "race-resistant known-good revalidation is unavailable on this platform",
        ))
    }
}

#[derive(Default)]
struct FilesystemContentReader {
    #[cfg(any(unix, windows))]
    inner: confined_fs::Reader,
}

impl ContentReader for FilesystemContentReader {
    fn hash_file(
        &self,
        path: &KnownGoodPhysicalPath,
        expected_size: u64,
        byte_budget: u64,
    ) -> ContentHashResult {
        #[cfg(any(unix, windows))]
        return self.inner.hash_file(path, expected_size, byte_budget);
        #[cfg(not(any(unix, windows)))]
        ContentHashResult {
            observation: Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "race-resistant known-good content reads are unavailable on this platform",
            )),
            bytes_read: 0,
        }
    }

    fn revalidate(&self) -> io::Result<()> {
        #[cfg(any(unix, windows))]
        return self.inner.revalidate();
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "race-resistant known-good revalidation is unavailable on this platform",
        ))
    }
}

#[derive(Debug, Default)]
pub(crate) struct IntegrityTier0Report {
    pub(crate) facts: Vec<ExecutionFact>,
    pub(crate) selected_entry_count: usize,
    pub(crate) skipped_bulk_entry_count: usize,
    pub(crate) metadata_lookup_count: usize,
    pub(crate) link_lookup_count: usize,
    pub(crate) mtime_observation_count: usize,
    pub(crate) suppressed_fact_count: usize,
}

pub(crate) fn sense_integrity_tier0(
    state: &AppState,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
    runtime_selection: LaunchTier0RuntimeSelection<'_>,
) -> Result<IntegrityTier0Report, KnownGoodVerificationUnavailable> {
    let lease = state.mint_known_good_verification_lease(lifecycle, expected_library_root)?;
    let report = sense_integrity_tier0_with(
        &lease,
        runtime_selection,
        &FilesystemMetadataReader::default(),
    );
    if !state.known_good_verification_lease_is_current(&lease) {
        return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
    }
    Ok(report)
}

fn sense_integrity_tier0_with(
    lease: &KnownGoodVerificationLease,
    runtime_selection: LaunchTier0RuntimeSelection<'_>,
    reader: &impl MetadataReader,
) -> IntegrityTier0Report {
    let (_instance_id, _version_id, _created_at, library_root, managed_runtime_cache, inventory) =
        lease.execution_parts();
    let mut report = IntegrityTier0Report::default();
    let projection = match inventory.launch_tier0_projection(runtime_selection) {
        Ok(projection) => projection,
        Err(error) => {
            report.selected_entry_count = error.selected_entry_count();
            push_bounded_fact(
                &mut report,
                projection_refused_fact(error.selected_entry_count()),
            );
            return report;
        }
    };
    report.selected_entry_count = projection.len();
    report.skipped_bulk_entry_count = inventory.entries().len() - projection.len();
    let mut sensed_facts = Vec::new();
    for (ordinal, entry) in projection {
        report.metadata_lookup_count += 1;
        let path = known_good_entry_path(library_root, managed_runtime_cache, entry);
        let fact = match reader.symlink_metadata(&path) {
            Ok(observation) => {
                report.mtime_observation_count += usize::from(observation.modified.is_some());
                inspect_observation(reader, entry, &path, ordinal, observation, &mut report)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(integrity_fact(
                entry,
                ordinal,
                ExecutionFactKind::ArtifactMissing,
                "missing",
            )),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Some(integrity_fact(
                entry,
                ordinal,
                ExecutionFactKind::FilePermissionDenied,
                "metadata_permission_denied",
            )),
            Err(_) => Some(integrity_fact(
                entry,
                ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "metadata_unavailable",
            )),
        };
        if let Some(fact) = fact {
            sensed_facts.push(fact);
        }
    }
    if reader.revalidate().is_err() {
        push_bounded_fact(&mut report, confinement_refused_fact());
    } else {
        for fact in normalize_runtime_facts(sensed_facts) {
            push_bounded_fact(&mut report, fact);
        }
    }
    report
}

const MAX_INTEGRITY_TIER1_FACTS: usize = 64;

struct Tier1HashJob {
    file: LaunchTier1AdmittedFile,
    inventory_ordinal: usize,
    path: KnownGoodPhysicalPath,
}

#[derive(Debug, Default)]
pub(crate) struct IntegrityTier1Report {
    pub(crate) facts: Vec<ExecutionFact>,
    pub(crate) hashed_entry_count: usize,
    pub(crate) content_read_byte_count: u64,
    pub(crate) suppressed_fact_count: usize,
}

pub(crate) async fn sense_integrity_tier1(
    state: &AppState,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
) -> Result<IntegrityTier1Report, KnownGoodVerificationUnavailable> {
    sense_integrity_tier1_with_reader_factory(
        state,
        lifecycle,
        expected_library_root,
        FilesystemContentReader::default,
    )
    .await
}

async fn sense_integrity_tier1_with_reader_factory<Factory, Reader>(
    state: &AppState,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
    reader_factory: Factory,
) -> Result<IntegrityTier1Report, KnownGoodVerificationUnavailable>
where
    Factory: FnOnce() -> Reader + Send + 'static,
    Reader: ContentReader,
{
    let lease = state.mint_known_good_verification_lease(lifecycle, expected_library_root)?;
    let prepared = prepare_tier1_jobs(&lease);
    let (lease, report) = match prepared {
        Ok(jobs) => tokio::task::spawn_blocking(move || {
            let report = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let reader = reader_factory();
                run_tier1_jobs(jobs, &reader)
            }))
            .unwrap_or_else(|_| tier1_worker_refused_report());
            (lease, report)
        })
        .await
        .map_err(|_| KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)?,
        Err(report) => (lease, report),
    };
    if !state.known_good_verification_lease_is_current(&lease) {
        return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
    }
    Ok(report)
}

#[cfg(test)]
fn sense_integrity_tier1_with(
    lease: &KnownGoodVerificationLease,
    reader: &impl ContentReader,
) -> IntegrityTier1Report {
    match prepare_tier1_jobs(lease) {
        Ok(jobs) => run_tier1_jobs(jobs, reader),
        Err(report) => report,
    }
}

fn prepare_tier1_jobs(
    lease: &KnownGoodVerificationLease,
) -> Result<Vec<Tier1HashJob>, IntegrityTier1Report> {
    let (_instance_id, _version_id, _created_at, library_root, _managed_runtime_cache, inventory) =
        lease.execution_parts();
    let projection = inventory.launch_tier1_projection().map_err(|error| {
        let mut report = IntegrityTier1Report::default();
        push_bounded_tier1_fact(
            &mut report,
            tier1_projection_refused_fact(error.selected_entry_count()),
        );
        report
    })?;
    let projected_entries = projection.into_entries();
    let mut jobs = Vec::with_capacity(projected_entries.len());
    for projected in projected_entries {
        let (inventory_ordinal, file) = projected.into_parts();
        jobs.push(Tier1HashJob {
            path: file.physical_path(library_root),
            file,
            inventory_ordinal,
        });
    }
    Ok(jobs)
}

fn run_tier1_jobs(jobs: Vec<Tier1HashJob>, reader: &impl ContentReader) -> IntegrityTier1Report {
    let mut report = IntegrityTier1Report::default();
    let mut sensed_facts = Vec::new();
    for job in jobs {
        let Some(byte_budget) =
            MAX_LAUNCH_TIER1_AGGREGATE_BYTES.checked_sub(report.content_read_byte_count)
        else {
            push_bounded_tier1_fact(&mut report, tier1_budget_accounting_refused_fact());
            return report;
        };
        let result = reader.hash_file(&job.path, job.file.size(), byte_budget);
        let Some(content_read_byte_count) = report
            .content_read_byte_count
            .checked_add(result.bytes_read)
        else {
            push_bounded_tier1_fact(&mut report, tier1_budget_accounting_refused_fact());
            return report;
        };
        report.content_read_byte_count = content_read_byte_count;
        if result.bytes_read > byte_budget {
            sensed_facts.push(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "content_budget_exceeded",
            ));
            break;
        }
        let fact = match result.observation {
            Ok(ContentHashObservation::Hashed { digest }) => {
                report.hashed_entry_count += 1;
                (digest != job.file.digest().as_str()).then(|| {
                    tier1_integrity_fact(
                        &job.file,
                        job.inventory_ordinal,
                        ExecutionFactKind::ArtifactHashMismatch,
                        "hash_mismatch",
                    )
                })
            }
            Ok(ContentHashObservation::SizeDrift { observed_size }) => {
                let mut fact = tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::ArtifactSizeDrift,
                    "size_drift",
                );
                fact.fields.extend([
                    public_field("expected_size", job.file.size().to_string()),
                    public_field("observed_size", observed_size.to_string()),
                ]);
                Some(fact)
            }
            Ok(ContentHashObservation::WrongType) => Some(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::ArtifactMissing,
                "wrong_type",
            )),
            Ok(ContentHashObservation::ChangedDuringRead) => Some(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "content_changed_during_read",
            )),
            Ok(ContentHashObservation::BudgetRefused) => Some(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "content_budget_refused",
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::ArtifactMissing,
                "missing",
            )),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                Some(tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::FilePermissionDenied,
                    "content_permission_denied",
                ))
            }
            Err(_) => Some(tier1_integrity_fact(
                &job.file,
                job.inventory_ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "content_unavailable",
            )),
        };
        if let Some(fact) = fact {
            sensed_facts.push(fact);
        }
    }
    if reader.revalidate().is_err() {
        push_bounded_tier1_fact(&mut report, tier1_confinement_refused_fact());
    } else {
        for fact in sensed_facts {
            push_bounded_tier1_fact(&mut report, fact);
        }
    }
    report
}

fn tier1_projection_refused_fact(selected_entry_count: usize) -> ExecutionFact {
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "known_good_suspicious_projection",
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![
            public_field("observation", "tier1_projection_refused"),
            public_field("selected_entry_count", selected_entry_count.to_string()),
        ],
    }
}

fn tier1_worker_refused_report() -> IntegrityTier1Report {
    let mut report = IntegrityTier1Report::default();
    push_bounded_tier1_fact(
        &mut report,
        ExecutionFact {
            operation_id: None,
            kind: ExecutionFactKind::PrimitiveRefused,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Artifact,
                "known_good_suspicious_worker",
                OwnershipClass::LauncherManaged,
            )),
            fields: vec![public_field("observation", "tier1_worker_unavailable")],
        },
    );
    report
}

fn tier1_confinement_refused_fact() -> ExecutionFact {
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "known_good_path_confinement",
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![public_field("observation", "path_identity_changed")],
    }
}

fn tier1_budget_accounting_refused_fact() -> ExecutionFact {
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "known_good_content_budget",
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![public_field(
            "observation",
            "content_budget_accounting_refused",
        )],
    }
}

fn push_bounded_tier1_fact(report: &mut IntegrityTier1Report, fact: ExecutionFact) {
    if report.facts.len() < MAX_INTEGRITY_TIER1_FACTS {
        report.facts.push(fact);
    } else {
        report.suppressed_fact_count += 1;
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RuntimeFactDisposition {
    Missing,
    MarkerOnly,
    Preserve,
}

#[derive(Default)]
struct RuntimeFactShape {
    manifest_issue: bool,
    marker_issue: bool,
    executable_issue: bool,
    non_metadata_issue: bool,
}

fn normalize_runtime_facts(facts: Vec<ExecutionFact>) -> Vec<ExecutionFact> {
    let mut shapes = BTreeMap::<String, RuntimeFactShape>::new();
    for fact in &facts {
        let Some(component) = fact_field(fact, "runtime_component") else {
            continue;
        };
        let shape = shapes.entry(component.to_string()).or_default();
        let metadata_issue = matches!(
            fact.kind,
            ExecutionFactKind::ArtifactMissing | ExecutionFactKind::ArtifactSizeDrift
        );
        if !metadata_issue {
            shape.non_metadata_issue = true;
            continue;
        }
        match fact_field(fact, "artifact_kind") {
            Some("runtime_manifest_proof") => shape.manifest_issue = true,
            Some("runtime_ready_marker") => shape.marker_issue = true,
            Some("runtime_executable") => shape.executable_issue = true,
            _ => shape.non_metadata_issue = true,
        }
    }
    let dispositions = shapes
        .into_iter()
        .map(|(component, shape)| {
            let disposition = if shape.non_metadata_issue {
                RuntimeFactDisposition::Preserve
            } else if shape.manifest_issue || shape.executable_issue {
                RuntimeFactDisposition::Missing
            } else if shape.marker_issue {
                RuntimeFactDisposition::MarkerOnly
            } else {
                RuntimeFactDisposition::Preserve
            };
            (component, disposition)
        })
        .collect::<BTreeMap<_, _>>();
    let mut emitted = BTreeSet::new();
    let mut normalized = Vec::with_capacity(facts.len());
    for mut fact in facts {
        let Some(component) = fact_field(&fact, "runtime_component").map(str::to_string) else {
            normalized.push(fact);
            continue;
        };
        match dispositions
            .get(&component)
            .copied()
            .unwrap_or(RuntimeFactDisposition::Preserve)
        {
            RuntimeFactDisposition::Preserve => normalized.push(fact),
            RuntimeFactDisposition::Missing => {
                if emitted.insert(component) {
                    fact.kind = ExecutionFactKind::RuntimeMissingExecutable;
                    fact.fields.retain(|field| {
                        matches!(field.key.as_str(), "inventory_root" | "runtime_component")
                    });
                    fact.fields
                        .push(public_field("observation", "runtime_structure_unavailable"));
                    normalized.push(fact);
                }
            }
            RuntimeFactDisposition::MarkerOnly => {
                if emitted.insert(component) {
                    fact.kind = ExecutionFactKind::RuntimeReadyMarkerMissing;
                    fact.fields.retain(|field| {
                        matches!(
                            field.key.as_str(),
                            "inventory_root" | "runtime_component" | "artifact_kind"
                        )
                    });
                    fact.fields
                        .push(public_field("observation", "ready_marker_unavailable"));
                    normalized.push(fact);
                }
            }
        }
    }
    normalized
}

fn fact_field<'a>(fact: &'a ExecutionFact, key: &str) -> Option<&'a str> {
    fact.fields
        .iter()
        .find(|field| field.key == key)
        .map(|field| field.value.as_str())
}

fn inspect_observation(
    reader: &impl MetadataReader,
    entry: &KnownGoodEntry,
    path: &KnownGoodPhysicalPath,
    ordinal: usize,
    observation: MetadataObservation,
    report: &mut IntegrityTier0Report,
) -> Option<ExecutionFact> {
    match entry.integrity() {
        KnownGoodIntegrity::Directory => (observation.kind != MetadataKind::Directory).then(|| {
            integrity_fact(
                entry,
                ordinal,
                ExecutionFactKind::ArtifactMissing,
                "wrong_type",
            )
        }),
        KnownGoodIntegrity::LinkTarget(_) => {
            if observation.kind != MetadataKind::Link {
                return Some(integrity_fact(
                    entry,
                    ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "wrong_type",
                ));
            }
            report.link_lookup_count += 1;
            match reader.read_link(path) {
                Ok(target) if known_good_link_target_matches(entry, &target) => None,
                Ok(_) => Some(integrity_fact(
                    entry,
                    ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "link_target_drift",
                )),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    Some(integrity_fact(
                        entry,
                        ordinal,
                        ExecutionFactKind::FilePermissionDenied,
                        "link_target_permission_denied",
                    ))
                }
                Err(_) => Some(integrity_fact(
                    entry,
                    ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "link_target_unavailable",
                )),
            }
        }
        KnownGoodIntegrity::Sha1 { size, .. } | KnownGoodIntegrity::ExactBytes { size, .. } => {
            if observation.kind != MetadataKind::File {
                return Some(integrity_fact(
                    entry,
                    ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "wrong_type",
                ));
            }
            (observation.size != *size).then(|| {
                let mut fact = integrity_fact(
                    entry,
                    ordinal,
                    ExecutionFactKind::ArtifactSizeDrift,
                    "size_drift",
                );
                fact.fields.extend([
                    public_field("expected_size", size.to_string()),
                    public_field("observed_size", observation.size.to_string()),
                ]);
                fact
            })
        }
    }
}

fn projection_refused_fact(selected_entry_count: usize) -> ExecutionFact {
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "known_good_launch_projection",
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![
            public_field("observation", "projection_oversized"),
            public_field("selected_entry_count", selected_entry_count.to_string()),
        ],
    }
}

fn confinement_refused_fact() -> ExecutionFact {
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            "known_good_path_confinement",
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![public_field("observation", "ancestor_identity_changed")],
    }
}

fn integrity_fact(
    entry: &KnownGoodEntry,
    ordinal: usize,
    kind: ExecutionFactKind,
    observation: &'static str,
) -> ExecutionFact {
    integrity_fact_from_parts(entry.root(), entry.kind(), ordinal, kind, observation)
}

fn tier1_integrity_fact(
    file: &LaunchTier1AdmittedFile,
    ordinal: usize,
    kind: ExecutionFactKind,
    observation: &'static str,
) -> ExecutionFact {
    integrity_fact_from_parts(file.root(), file.kind(), ordinal, kind, observation)
}

fn integrity_fact_from_parts(
    entry_root: &KnownGoodRoot,
    entry_kind: axial_minecraft::known_good::KnownGoodArtifactKind,
    ordinal: usize,
    kind: ExecutionFactKind,
    observation: &'static str,
) -> ExecutionFact {
    let root = entry_root.stable_id();
    let artifact_kind = entry_kind.stable_id();
    let mut fact = ExecutionFact {
        operation_id: None,
        kind,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            if matches!(entry_root, KnownGoodRoot::ManagedRuntime { .. }) {
                TargetKind::Runtime
            } else {
                TargetKind::Artifact
            },
            format!("known_good_{root}_{artifact_kind}_{ordinal}"),
            OwnershipClass::LauncherManaged,
        )),
        fields: vec![
            public_field("inventory_root", root),
            public_field("artifact_kind", artifact_kind),
            public_field("entry_ordinal", ordinal.to_string()),
            public_field("observation", observation),
        ],
    };
    if let KnownGoodRoot::ManagedRuntime { component } = entry_root {
        fact.fields
            .push(public_field("runtime_component", component.as_str()));
    }
    fact
}

fn public_field(key: impl Into<String>, value: impl Into<String>) -> EvidenceField {
    EvidenceField::new(key, value, EvidenceSensitivity::Public)
}

fn push_bounded_fact(report: &mut IntegrityTier0Report, fact: ExecutionFact) {
    if report.facts.len() < MAX_INTEGRITY_TIER0_FACTS {
        report.facts.push(fact);
    } else {
        report.suppressed_fact_count += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::application::timing::INTEGRITY_TIER0_CEILING_MS;
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity, TestKnownGoodRoot,
    };
    use axial_performance::PerformanceManager;
    use std::collections::HashMap;
    use std::fs;
    use std::sync::{Arc, Condvar, Mutex};
    use std::time::{Duration, Instant};

    #[derive(Clone, Copy)]
    enum ScriptedMetadata {
        Present(MetadataObservation),
        Error(io::ErrorKind),
    }

    struct ScriptedReader {
        metadata: HashMap<String, ScriptedMetadata>,
        links: HashMap<String, Result<PathBuf, io::ErrorKind>>,
        metadata_paths: Mutex<Vec<PathBuf>>,
        link_paths: Mutex<Vec<PathBuf>>,
        revalidate_error: Option<io::ErrorKind>,
    }

    impl ScriptedReader {
        fn new(
            metadata: impl IntoIterator<Item = (&'static str, ScriptedMetadata)>,
            links: impl IntoIterator<Item = (&'static str, Result<&'static str, io::ErrorKind>)>,
        ) -> Self {
            Self {
                metadata: metadata
                    .into_iter()
                    .map(|(suffix, observation)| (suffix.to_string(), observation))
                    .collect(),
                links: links
                    .into_iter()
                    .map(|(suffix, target)| (suffix.to_string(), target.map(PathBuf::from)))
                    .collect(),
                metadata_paths: Mutex::new(Vec::new()),
                link_paths: Mutex::new(Vec::new()),
                revalidate_error: None,
            }
        }

        fn with_revalidate_error(mut self, kind: io::ErrorKind) -> Self {
            self.revalidate_error = Some(kind);
            self
        }

        fn matching<T: Clone>(path: &Path, values: &HashMap<String, T>) -> Option<T> {
            values
                .iter()
                .find_map(|(suffix, value)| path.ends_with(suffix).then(|| value.clone()))
        }
    }

    impl MetadataReader for ScriptedReader {
        fn symlink_metadata(
            &self,
            path: &KnownGoodPhysicalPath,
        ) -> io::Result<MetadataObservation> {
            let path = path.root().join(path.relative());
            self.metadata_paths
                .lock()
                .expect("metadata paths")
                .push(path.clone());
            match Self::matching(&path, &self.metadata) {
                Some(ScriptedMetadata::Present(observation)) => Ok(observation),
                Some(ScriptedMetadata::Error(kind)) => Err(io::Error::from(kind)),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }

        fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf> {
            let path = path.root().join(path.relative());
            self.link_paths
                .lock()
                .expect("link paths")
                .push(path.clone());
            match Self::matching(&path, &self.links) {
                Some(Ok(target)) => Ok(target),
                Some(Err(kind)) => Err(io::Error::from(kind)),
                None => Err(io::Error::from(io::ErrorKind::NotFound)),
            }
        }

        fn revalidate(&self) -> io::Result<()> {
            self.revalidate_error
                .map_or(Ok(()), |kind| Err(io::Error::from(kind)))
        }
    }

    #[derive(Clone, Copy)]
    enum ScriptedContent {
        Hashed(&'static str, u64),
        SizeDriftAfterRead {
            observed_size: u64,
            bytes_read: u64,
        },
        WrongType,
        ChangedDuringRead,
        Error(io::ErrorKind),
        ErrorAfterRead {
            kind: io::ErrorKind,
            bytes_read: u64,
        },
    }

    struct ScriptedContentReader {
        content: HashMap<String, ScriptedContent>,
        default: ScriptedContent,
        content_paths: Mutex<Vec<(PathBuf, u64, u64)>>,
        revalidate_error: Option<io::ErrorKind>,
    }

    impl ScriptedContentReader {
        fn new(content: impl IntoIterator<Item = (&'static str, ScriptedContent)>) -> Self {
            Self {
                content: content
                    .into_iter()
                    .map(|(suffix, observation)| (suffix.to_string(), observation))
                    .collect(),
                default: ScriptedContent::Error(io::ErrorKind::NotFound),
                content_paths: Mutex::new(Vec::new()),
                revalidate_error: None,
            }
        }

        fn with_default(mut self, default: ScriptedContent) -> Self {
            self.default = default;
            self
        }

        fn with_revalidate_error(mut self, kind: io::ErrorKind) -> Self {
            self.revalidate_error = Some(kind);
            self
        }
    }

    impl ContentReader for ScriptedContentReader {
        fn hash_file(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            let path = path.root().join(path.relative());
            self.content_paths.lock().expect("content paths").push((
                path.clone(),
                expected_size,
                byte_budget,
            ));
            let (observation, bytes_read) =
                match ScriptedReader::matching(&path, &self.content).unwrap_or(self.default) {
                    ScriptedContent::Hashed(digest, size) if size <= byte_budget => (
                        Ok(ContentHashObservation::Hashed {
                            digest: digest.to_string(),
                        }),
                        size,
                    ),
                    ScriptedContent::Hashed(_, _) => (Ok(ContentHashObservation::BudgetRefused), 0),
                    ScriptedContent::SizeDriftAfterRead {
                        observed_size,
                        bytes_read,
                    } if bytes_read <= byte_budget => (
                        Ok(ContentHashObservation::SizeDrift { observed_size }),
                        bytes_read,
                    ),
                    ScriptedContent::SizeDriftAfterRead { .. } => {
                        (Ok(ContentHashObservation::BudgetRefused), 0)
                    }
                    ScriptedContent::WrongType => (Ok(ContentHashObservation::WrongType), 0),
                    ScriptedContent::ChangedDuringRead => {
                        (Ok(ContentHashObservation::ChangedDuringRead), 0)
                    }
                    ScriptedContent::Error(kind) => (Err(io::Error::from(kind)), 0),
                    ScriptedContent::ErrorAfterRead { kind, bytes_read }
                        if bytes_read <= byte_budget =>
                    {
                        (Err(io::Error::from(kind)), bytes_read)
                    }
                    ScriptedContent::ErrorAfterRead { .. } => {
                        (Ok(ContentHashObservation::BudgetRefused), 0)
                    }
                };
            ContentHashResult {
                observation,
                bytes_read,
            }
        }

        fn revalidate(&self) -> io::Result<()> {
            self.revalidate_error
                .map_or(Ok(()), |kind| Err(io::Error::from(kind)))
        }
    }

    struct BlockingContentGate {
        state: Mutex<BlockingContentGateState>,
        released: Condvar,
    }

    struct BlockingContentGateState {
        started: Option<tokio::sync::oneshot::Sender<()>>,
        released: bool,
    }

    impl BlockingContentGate {
        fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
            let (started, observed) = tokio::sync::oneshot::channel();
            (
                Arc::new(Self {
                    state: Mutex::new(BlockingContentGateState {
                        started: Some(started),
                        released: false,
                    }),
                    released: Condvar::new(),
                }),
                observed,
            )
        }

        fn wait(&self) {
            let mut state = self.state.lock().expect("blocking content gate");
            if let Some(started) = state.started.take() {
                let _ = started.send(());
            }
            while !state.released {
                state = self.released.wait(state).expect("blocking content release");
            }
        }

        fn release(&self) {
            let mut state = self.state.lock().expect("blocking content gate");
            state.released = true;
            self.released.notify_all();
        }
    }

    struct BlockingContentReader {
        gate: Arc<BlockingContentGate>,
    }

    impl ContentReader for BlockingContentReader {
        fn hash_file(
            &self,
            _path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            self.gate.wait();
            if expected_size > byte_budget {
                return ContentHashResult {
                    observation: Ok(ContentHashObservation::BudgetRefused),
                    bytes_read: 0,
                };
            }
            ContentHashResult {
                observation: Ok(ContentHashObservation::Hashed {
                    digest: ZERO_SHA1.to_string(),
                }),
                bytes_read: expected_size,
            }
        }

        fn revalidate(&self) -> io::Result<()> {
            Ok(())
        }
    }

    #[cfg(unix)]
    struct BlockingFilesystemContentReader {
        inner: FilesystemContentReader,
        blocked_leaf: PathBuf,
        gate: Arc<BlockingContentGate>,
    }

    #[cfg(unix)]
    impl ContentReader for BlockingFilesystemContentReader {
        fn hash_file(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            if path.relative().ends_with(&self.blocked_leaf) {
                self.gate.wait();
            }
            self.inner.hash_file(path, expected_size, byte_budget)
        }

        fn revalidate(&self) -> io::Result<()> {
            self.inner.revalidate()
        }
    }

    fn observation(kind: MetadataKind, size: u64) -> ScriptedMetadata {
        ScriptedMetadata::Present(MetadataObservation {
            kind,
            size,
            modified: Some(SystemTime::UNIX_EPOCH),
        })
    }

    fn runtime_metadata_fact(
        kind: ExecutionFactKind,
        artifact_kind: &'static str,
    ) -> ExecutionFact {
        ExecutionFact {
            operation_id: None,
            kind,
            target: Some(TargetDescriptor::new(
                StabilizationSystem::Execution,
                TargetKind::Runtime,
                "known_good_runtime_test",
                OwnershipClass::LauncherManaged,
            )),
            fields: vec![
                public_field("inventory_root", "managed_runtime"),
                public_field("artifact_kind", artifact_kind),
                public_field("runtime_component", "java-runtime-delta"),
                public_field("observation", "missing"),
            ],
        }
    }

    #[test]
    fn absent_runtime_structure_normalizes_to_one_recoverable_runtime_fact() {
        let facts = normalize_runtime_facts(vec![
            runtime_metadata_fact(ExecutionFactKind::ArtifactMissing, "runtime_manifest_proof"),
            runtime_metadata_fact(ExecutionFactKind::ArtifactMissing, "runtime_ready_marker"),
            runtime_metadata_fact(ExecutionFactKind::ArtifactMissing, "runtime_executable"),
        ]);

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].kind, ExecutionFactKind::RuntimeMissingExecutable);
        assert!(facts[0].fields.iter().any(|field| {
            field.key == "observation" && field.value == "runtime_structure_unavailable"
        }));
    }

    #[test]
    fn isolated_ready_marker_drift_normalizes_to_marker_repair_fact() {
        let facts = normalize_runtime_facts(vec![runtime_metadata_fact(
            ExecutionFactKind::ArtifactMissing,
            "runtime_ready_marker",
        )]);

        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].kind, ExecutionFactKind::RuntimeReadyMarkerMissing);
    }

    fn test_paths(root: &Path, library_dir: PathBuf) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir,
            config_dir,
        }
    }

    fn state_fixture(label: &str, library_dir: Option<PathBuf>) -> (AppState, PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "axial-integrity-tier0-{label}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .expect("clock")
                .as_nanos()
        ));
        let library_dir = library_dir.unwrap_or_else(|| root.join("private-library-root"));
        fs::create_dir_all(&library_dir).expect("library root");
        let paths = test_paths(&root, library_dir.clone());
        let config = Arc::new(
            ConfigStore::from_config(
                paths.clone(),
                AppConfig {
                    library_dir: library_dir.to_string_lossy().into_owned(),
                    ..AppConfig::default()
                },
            )
            .expect("test config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
                .expect("test instances"),
        );
        let state = AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup(&paths.config_dir).expect("test performance"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        });
        (state, root)
    }

    fn entry(
        root: TestKnownGoodRoot,
        path: &str,
        kind: KnownGoodArtifactKind,
        integrity: TestKnownGoodIntegrity,
    ) -> TestKnownGoodEntry {
        TestKnownGoodEntry {
            root,
            path: path.to_string(),
            kind,
            integrity,
        }
    }

    async fn close_fixture(state: AppState, root: PathBuf) {
        state
            .close_known_good_inventories()
            .await
            .expect("close known-good store");
        state
            .close_instance_registry()
            .await
            .expect("close instance store");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";
    const NONZERO_SHA1: &str = "1111111111111111111111111111111111111111";

    #[test]
    fn exact_content_reader_never_consumes_bytes_beyond_the_admitted_size() {
        let mut content = std::io::Cursor::new(vec![7_u8; (64 * 1024) + 3]);
        let result = read_exact_sha1(&mut content, 3, 3);

        assert!(matches!(
            result.observation,
            Ok(ContentHashObservation::Hashed { .. })
        ));
        assert_eq!(result.bytes_read, 3);
        assert_eq!(content.position(), 3);

        let refused = read_exact_sha1(&mut content, 4, 3);
        assert!(matches!(
            refused.observation,
            Ok(ContentHashObservation::BudgetRefused)
        ));
        assert_eq!(refused.bytes_read, 0);
        assert_eq!(content.position(), 3);
    }

    #[tokio::test]
    async fn tier_one_hashes_exact_launch_content_and_healthy_content_is_silent() {
        let (state, root) = state_fixture("tier1-exact-projection", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one projection", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.jar",
                KnownGoodArtifactKind::ClientJar,
                TestKnownGoodIntegrity::File { size: 10 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "org/example/library.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 11 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "org/example/native.jar",
                KnownGoodArtifactKind::NativeLibrary,
                TestKnownGoodIntegrity::File { size: 12 },
            ),
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.json",
                KnownGoodArtifactKind::VersionMetadata,
                TestKnownGoodIntegrity::File { size: 13 },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "indexes/1.21.json",
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::File { size: 14 },
            ),
            entry(
                TestKnownGoodRoot::ManagedRuntime {
                    component: "java-runtime-delta".to_string(),
                },
                "bin/java",
                KnownGoodArtifactKind::RuntimeExecutable,
                TestKnownGoodIntegrity::File { size: 15 },
            ),
        ])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new([
            ("1.21.5/1.21.5.jar", ScriptedContent::Hashed(ZERO_SHA1, 10)),
            (
                "org/example/library.jar",
                ScriptedContent::Hashed(ZERO_SHA1, 11),
            ),
            (
                "org/example/native.jar",
                ScriptedContent::Hashed(ZERO_SHA1, 12),
            ),
        ]);

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 3);
        assert_eq!(report.content_read_byte_count, 33);
        assert_eq!(report.suppressed_fact_count, 0);
        assert!(report.facts.is_empty());
        {
            let content_paths = reader.content_paths.lock().expect("content paths");
            assert_eq!(content_paths.len(), 3);
            assert!(
                content_paths
                    .iter()
                    .any(|(path, size, _)| path.ends_with("1.21.5/1.21.5.jar") && *size == 10)
            );
            assert!(
                content_paths
                    .iter()
                    .any(|(path, size, _)| path.ends_with("org/example/library.jar") && *size == 11)
            );
            assert!(
                content_paths
                    .iter()
                    .any(|(path, size, _)| path.ends_with("org/example/native.jar") && *size == 12)
            );
            assert!(
                content_paths.iter().all(|(path, _, _)| {
                    !path.ends_with("1.21.5/1.21.5.json")
                        && !path.ends_with("indexes/1.21.json")
                        && !path.ends_with("bin/java")
                }),
                "Tier one must not expand beyond client, library, and native content"
            );
        }
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_reports_same_size_digest_mismatch_without_sensitive_evidence() {
        let (state, root) = state_fixture("tier1-hash-mismatch", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one mismatch", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "private/vendor/secret-library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 7 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new([(
            "private/vendor/secret-library.jar",
            ScriptedContent::Hashed(NONZERO_SHA1, 7),
        )]);

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 1);
        assert_eq!(report.content_read_byte_count, 7);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(
            report.facts[0].kind,
            ExecutionFactKind::ArtifactHashMismatch
        );
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("hash_mismatch")
        );
        let exported = serde_json::to_string(&report.facts).expect("facts json");
        assert!(!exported.contains("secret-library.jar"));
        assert!(!exported.contains("private-library-root"));
        assert!(!exported.contains(ZERO_SHA1));
        assert!(!exported.contains(NONZERO_SHA1));
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_classifies_content_read_failures_without_leaking_paths() {
        let (state, root) = state_fixture("tier1-classification", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one classification", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Libraries,
                "sensitive/missing.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "sensitive/size-drift.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "sensitive/wrong-type.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "sensitive/permission.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "sensitive/changed.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
        ])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new([
            (
                "sensitive/missing.jar",
                ScriptedContent::Error(io::ErrorKind::NotFound),
            ),
            (
                "sensitive/size-drift.jar",
                ScriptedContent::SizeDriftAfterRead {
                    observed_size: 9,
                    bytes_read: 7,
                },
            ),
            ("sensitive/wrong-type.jar", ScriptedContent::WrongType),
            (
                "sensitive/permission.jar",
                ScriptedContent::ErrorAfterRead {
                    kind: io::ErrorKind::PermissionDenied,
                    bytes_read: 3,
                },
            ),
            ("sensitive/changed.jar", ScriptedContent::ChangedDuringRead),
        ]);

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 0);
        assert_eq!(report.content_read_byte_count, 10);
        assert_eq!(report.facts.len(), 5);
        let fact_for = |observation| {
            report
                .facts
                .iter()
                .find(|fact| fact_field(fact, "observation") == Some(observation))
                .unwrap_or_else(|| panic!("missing {observation} fact"))
        };
        assert_eq!(fact_for("missing").kind, ExecutionFactKind::ArtifactMissing);
        let size_drift = fact_for("size_drift");
        assert_eq!(size_drift.kind, ExecutionFactKind::ArtifactSizeDrift);
        assert_eq!(fact_field(size_drift, "expected_size"), Some("7"));
        assert_eq!(fact_field(size_drift, "observed_size"), Some("9"));
        assert_eq!(
            fact_for("wrong_type").kind,
            ExecutionFactKind::ArtifactMissing
        );
        assert_eq!(
            fact_for("content_permission_denied").kind,
            ExecutionFactKind::FilePermissionDenied
        );
        assert_eq!(
            fact_for("content_changed_during_read").kind,
            ExecutionFactKind::PrimitiveRefused
        );
        let size_drift_budget = {
            let content_paths = reader.content_paths.lock().expect("content paths");
            content_paths
                .iter()
                .find_map(|(path, _, budget)| {
                    path.ends_with("sensitive/size-drift.jar")
                        .then_some(*budget)
                })
                .expect("size drift read budget")
        };
        assert_eq!(
            size_drift_budget,
            MAX_LAUNCH_TIER1_AGGREGATE_BYTES - 3,
            "partial permission failure bytes must reduce the next physical read budget"
        );
        let exported = serde_json::to_string(&report.facts).expect("facts json");
        for sensitive in [
            "sensitive/",
            "missing.jar",
            "size-drift.jar",
            "wrong-type.jar",
            "permission.jar",
            "changed.jar",
            "private-library-root",
        ] {
            assert!(!exported.contains(sensitive), "leaked {sensitive}");
        }
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_hashes_every_selected_entry_but_bounds_emitted_facts() {
        let (state, root) = state_fixture("tier1-fact-bound", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one bound", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries((0..70).map(|index| {
            entry(
                TestKnownGoodRoot::Libraries,
                &format!("bounded/{index:03}.jar"),
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 1 },
            )
        }))
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new(std::iter::empty())
            .with_default(ScriptedContent::Hashed(NONZERO_SHA1, 1));

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 70);
        assert_eq!(report.content_read_byte_count, 70);
        assert_eq!(
            reader.content_paths.lock().expect("content paths").len(),
            70
        );
        assert_eq!(report.facts.len(), MAX_INTEGRITY_TIER1_FACTS);
        assert_eq!(report.suppressed_fact_count, 6);
        assert!(
            report
                .facts
                .iter()
                .all(|fact| fact.kind == ExecutionFactKind::ArtifactHashMismatch)
        );
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn oversized_tier_one_projection_refuses_without_content_reads() {
        let (state, root) = state_fixture("tier1-projection-bound", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one projection bound", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries(
            (0..=axial_minecraft::known_good::MAX_LAUNCH_TIER1_ENTRIES).map(|index| {
                entry(
                    TestKnownGoodRoot::Libraries,
                    &format!("oversized-tier1/{index:03}.jar"),
                    KnownGoodArtifactKind::Library,
                    TestKnownGoodIntegrity::File { size: 1 },
                )
            }),
        )
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new(std::iter::empty());

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 0);
        assert_eq!(report.content_read_byte_count, 0);
        assert!(
            reader
                .content_paths
                .lock()
                .expect("content paths")
                .is_empty()
        );
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("tier1_projection_refused")
        );
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_ancestor_drift_discards_prior_hash_observations() {
        let (state, root) = state_fixture("tier1-ancestor-drift", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one ancestor drift", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "stable/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 7 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedContentReader::new([(
            "stable/library.jar",
            ScriptedContent::Hashed(NONZERO_SHA1, 7),
        )])
        .with_revalidate_error(io::ErrorKind::PermissionDenied);

        let report = sense_integrity_tier1_with(&lease, &reader);

        assert_eq!(report.hashed_entry_count, 1);
        assert_eq!(report.content_read_byte_count, 7);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("path_identity_changed")
        );
        assert!(
            report
                .facts
                .iter()
                .all(|fact| fact.kind != ExecutionFactKind::ArtifactHashMismatch)
        );
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn aborted_tier_one_caller_retains_lifecycle_until_blocking_worker_finishes() {
        let (state, root) = state_fixture("tier1-abort-retains-authority", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one abort", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "blocking/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 7 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let (gate, started) = BlockingContentGate::new();
        let sensing_state = state.clone();
        let sensing_instance_id = instance.id.clone();
        let sensing_library_root = root.join("private-library-root");
        let sensing_gate = gate.clone();
        let sensing = tokio::spawn(async move {
            let lifecycle = sensing_state
                .acquire_instance_lifecycle(&sensing_instance_id)
                .await;
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &lifecycle,
                &sensing_library_root,
                move || BlockingContentReader { gate: sensing_gate },
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), started)
            .await
            .expect("blocking worker started")
            .expect("blocking worker signal");
        sensing.abort();
        let cancellation = sensing.await.expect_err("sensing caller must be aborted");
        assert!(cancellation.is_cancelled());

        let mut lifecycle_mutation = Box::pin(state.acquire_instance_lifecycle(&instance.id));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut lifecycle_mutation)
                .await
                .is_err(),
            "instance lifecycle must remain held by the blocking worker"
        );

        gate.release();
        let lifecycle = tokio::time::timeout(Duration::from_secs(2), &mut lifecycle_mutation)
            .await
            .expect("blocking worker released lifecycle");
        drop(lifecycle);
        drop(lifecycle_mutation);
        close_fixture(state, root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn tier_one_discards_early_leaf_observations_when_path_is_replaced_later() {
        let (state, root) = state_fixture("tier1-leaf-replacement", None);
        let library_root = root.join("private-library-root");
        let managed = library_root.join("libraries/race");
        fs::create_dir_all(&managed).expect("managed library directory");
        let first = managed.join("first.jar");
        let displaced = managed.join("first.old");
        fs::write(&first, b"1234567").expect("first library");
        fs::write(managed.join("second.jar"), b"7654321").expect("second library");

        let instance = state
            .instances()
            .insert_for_test("Tier one leaf race", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Libraries,
                "race/first.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "race/second.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
        ])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let (gate, second_started) = BlockingContentGate::new();
        let sensing_state = state.clone();
        let sensing_instance_id = instance.id.clone();
        let sensing_library_root = library_root.clone();
        let sensing_gate = gate.clone();
        let sensing = tokio::spawn(async move {
            let lifecycle = sensing_state
                .acquire_instance_lifecycle(&sensing_instance_id)
                .await;
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &lifecycle,
                &sensing_library_root,
                move || BlockingFilesystemContentReader {
                    inner: FilesystemContentReader::default(),
                    blocked_leaf: PathBuf::from("race/second.jar"),
                    gate: sensing_gate,
                },
            )
            .await
        });

        tokio::time::timeout(Duration::from_secs(2), second_started)
            .await
            .expect("second hash reached")
            .expect("second hash signal");
        fs::rename(&first, &displaced).expect("displace hashed leaf");
        fs::write(&first, b"abcdefg").expect("replace hashed leaf");
        gate.release();

        let report = tokio::time::timeout(Duration::from_secs(2), sensing)
            .await
            .expect("Tier one sensing completed")
            .expect("Tier one sensing task")
            .expect("Tier one report");
        assert_eq!(report.hashed_entry_count, 2);
        assert_eq!(report.content_read_byte_count, 14);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("path_identity_changed")
        );
        assert!(
            report
                .facts
                .iter()
                .all(|fact| fact.kind != ExecutionFactKind::ArtifactHashMismatch)
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_zero_is_metadata_only_exact_bounded_and_redacted() {
        let (state, root) = state_fixture("contracts", None);
        let instance = state
            .instances()
            .insert_for_test("Integrity", "1.21.5")
            .expect("instance");
        let runtime_root = || TestKnownGoodRoot::ManagedRuntime {
            component: "java-runtime-delta".to_string(),
        };
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Assets,
                "indexes/1.21.json",
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::File { size: 20 },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "objects/00/0000000000000000000000000000000000000000",
                KnownGoodArtifactKind::AssetObject,
                TestKnownGoodIntegrity::File { size: 99 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "wrong-type.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 10 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "wrong-symlink.jar",
                KnownGoodArtifactKind::NativeLibrary,
                TestKnownGoodIntegrity::File { size: 10 },
            ),
            entry(
                runtime_root(),
                ".axial-ready",
                KnownGoodArtifactKind::RuntimeReadyMarker,
                TestKnownGoodIntegrity::ExactBytes { size: 5 },
            ),
            entry(
                runtime_root(),
                ".axial-runtime-manifest.json",
                KnownGoodArtifactKind::RuntimeManifestProof,
                TestKnownGoodIntegrity::ExactBytes { size: 30 },
            ),
            entry(
                runtime_root(),
                "bin",
                KnownGoodArtifactKind::RuntimeDirectory,
                TestKnownGoodIntegrity::Directory,
            ),
            entry(
                runtime_root(),
                "bin/java",
                KnownGoodArtifactKind::RuntimeExecutable,
                TestKnownGoodIntegrity::File { size: 40 },
            ),
            entry(
                runtime_root(),
                "java-link",
                KnownGoodArtifactKind::RuntimeLink,
                TestKnownGoodIntegrity::LinkTarget("bin/java".to_string()),
            ),
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.jar",
                KnownGoodArtifactKind::ClientJar,
                TestKnownGoodIntegrity::File { size: 10 },
            ),
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.json",
                KnownGoodArtifactKind::VersionMetadata,
                TestKnownGoodIntegrity::File { size: 15 },
            ),
        ])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("exact live lease");
        let normalized_root =
            fs::canonicalize(root.join("private-library-root")).expect("canonical root");
        assert_eq!(
            lease.exact_identity_for_test(),
            (
                instance.id.as_str(),
                instance.version_id.as_str(),
                instance.created_at.as_str(),
                normalized_root.as_path(),
            )
        );
        let reader = ScriptedReader::new(
            [
                ("indexes/1.21.json", observation(MetadataKind::File, 21)),
                ("wrong-type.jar", observation(MetadataKind::Directory, 0)),
                ("wrong-symlink.jar", observation(MetadataKind::Link, 0)),
                (".axial-ready", observation(MetadataKind::File, 5)),
                (
                    ".axial-runtime-manifest.json",
                    observation(MetadataKind::File, 30),
                ),
                ("bin", observation(MetadataKind::Directory, 0)),
                ("bin/java", observation(MetadataKind::File, 40)),
                ("java-link", observation(MetadataKind::Link, 0)),
                ("1.21.5.jar", observation(MetadataKind::File, 10)),
                (
                    "1.21.5.json",
                    ScriptedMetadata::Error(io::ErrorKind::NotFound),
                ),
            ],
            [("java-link", Ok("./bin/../bin/java"))],
        );
        let report = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &reader,
        );

        assert_eq!(report.selected_entry_count, 8);
        assert_eq!(report.skipped_bulk_entry_count, 3);
        assert_eq!(report.metadata_lookup_count, 8);
        assert_eq!(report.link_lookup_count, 0);
        assert_eq!(report.mtime_observation_count, 7);
        assert_eq!(report.suppressed_fact_count, 0);
        assert_eq!(reader.metadata_paths.lock().expect("paths").len(), 8);
        assert_eq!(reader.link_paths.lock().expect("links").len(), 0);
        assert_eq!(
            report
                .facts
                .iter()
                .map(|fact| fact.kind)
                .collect::<Vec<_>>(),
            [
                ExecutionFactKind::ArtifactSizeDrift,
                ExecutionFactKind::ArtifactMissing,
                ExecutionFactKind::ArtifactMissing,
                ExecutionFactKind::ArtifactMissing,
            ]
        );
        let exported = serde_json::to_string(&report.facts).expect("facts json");
        assert!(!exported.contains("private-library-root"));
        assert!(!exported.contains("wrong-type.jar"));
        assert!(!exported.contains("wrong-symlink.jar"));
        assert!(!exported.contains("1.21.5.json"));
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_zero_senses_every_selected_entry_but_bounds_emitted_facts() {
        let (state, root) = state_fixture("fact-bound", None);
        let instance = state
            .instances()
            .insert_for_test("Bound", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries((0..70).map(|index| {
            entry(
                TestKnownGoodRoot::Libraries,
                &format!("bounded/{index:03}.jar"),
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 1 },
            )
        }))
        .expect("bounded inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedReader::new(
            std::iter::empty::<(&str, ScriptedMetadata)>(),
            std::iter::empty::<(&str, Result<&str, io::ErrorKind>)>(),
        );
        let report = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &reader,
        );
        assert_eq!(report.metadata_lookup_count, 70);
        assert_eq!(report.facts.len(), MAX_INTEGRITY_TIER0_FACTS);
        assert_eq!(report.suppressed_fact_count, 6);
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn oversized_launch_projection_fails_closed_without_filesystem_work() {
        let (state, root) = state_fixture("projection-bound", None);
        let instance = state
            .instances()
            .insert_for_test("Projection bound", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries(
            (0..=axial_minecraft::known_good::MAX_LAUNCH_TIER0_ENTRIES).map(|index| {
                entry(
                    TestKnownGoodRoot::Libraries,
                    &format!("oversized/{index:03}.jar"),
                    KnownGoodArtifactKind::Library,
                    TestKnownGoodIntegrity::File { size: 1 },
                )
            }),
        )
        .expect("oversized inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedReader::new(
            std::iter::empty::<(&str, ScriptedMetadata)>(),
            std::iter::empty::<(&str, Result<&str, io::ErrorKind>)>(),
        );
        let report = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &reader,
        );
        assert_eq!(
            report.selected_entry_count,
            axial_minecraft::known_good::MAX_LAUNCH_TIER0_ENTRIES + 1
        );
        assert_eq!(report.metadata_lookup_count, 0);
        assert_eq!(report.link_lookup_count, 0);
        assert!(reader.metadata_paths.lock().expect("paths").is_empty());
        assert!(reader.link_paths.lock().expect("links").is_empty());
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert!(
            report.facts[0].fields.iter().any(|field| {
                field.key == "observation" && field.value == "projection_oversized"
            })
        );
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn ancestor_identity_drift_discards_all_prior_observations() {
        let (state, root) = state_fixture("ancestor-drift", None);
        let instance = state
            .instances()
            .insert_for_test("Ancestor drift", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "stable/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 7 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let reader = ScriptedReader::new(
            [("stable/library.jar", observation(MetadataKind::File, 7))],
            std::iter::empty::<(&str, Result<&str, io::ErrorKind>)>(),
        )
        .with_revalidate_error(io::ErrorKind::PermissionDenied);

        let report = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &reader,
        );

        assert_eq!(report.metadata_lookup_count, 1);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert!(report.facts[0].fields.iter().any(|field| {
            field.key == "observation" && field.value == "ancestor_identity_changed"
        }));
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn platform_java_link_target_is_verified_without_content_io() {
        let (state, root) = state_fixture("java-link", None);
        let instance = state
            .instances()
            .insert_for_test("Java link", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::ManagedRuntime {
                component: "java-runtime-delta".to_string(),
            },
            "bin/java",
            KnownGoodArtifactKind::RuntimeExecutable,
            TestKnownGoodIntegrity::LinkTarget("java-real".to_string()),
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease = state
            .mint_known_good_verification_lease(&lifecycle, &root.join("private-library-root"))
            .expect("lease");
        let healthy_reader = ScriptedReader::new(
            [("bin/java", observation(MetadataKind::Link, 0))],
            [("bin/java", Ok("./java-real"))],
        );

        let healthy = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &healthy_reader,
        );
        assert!(healthy.facts.is_empty());
        assert_eq!(healthy.metadata_lookup_count, 1);
        assert_eq!(healthy.link_lookup_count, 1);

        let drifted_reader = ScriptedReader::new(
            [("bin/java", observation(MetadataKind::Link, 0))],
            [("bin/java", Ok("different-java"))],
        );
        let drifted = sense_integrity_tier0_with(
            &lease,
            LaunchTier0RuntimeSelection::PreferredManaged,
            &drifted_reader,
        );
        assert_eq!(drifted.facts.len(), 1);
        assert_eq!(
            drifted.facts[0].kind,
            ExecutionFactKind::RuntimeMissingExecutable
        );
        assert!(drifted.facts[0].fields.iter().any(|field| {
            field.key == "observation" && field.value == "runtime_structure_unavailable"
        }));
        drop(lease);
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn filesystem_sensor_never_follows_symlinked_managed_ancestor_or_leaf() {
        use std::os::unix::fs::symlink;

        let (state, root) = state_fixture("symlink-confinement", None);
        let library_root = root.join("private-library-root");
        let libraries = library_root.join("libraries");
        let outside = root.join("user-owned-outside");
        fs::create_dir_all(&libraries).expect("libraries root");
        fs::create_dir_all(&outside).expect("outside root");
        fs::write(outside.join("managed.jar"), b"1234567").expect("outside ancestor file");
        fs::write(outside.join("leaf.jar"), b"1234567").expect("outside leaf file");
        symlink(&outside, libraries.join("ancestor")).expect("ancestor symlink");
        symlink(outside.join("leaf.jar"), libraries.join("leaf.jar")).expect("leaf symlink");

        let instance = state
            .instances()
            .insert_for_test("Symlink confinement", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Libraries,
                "ancestor/managed.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "leaf.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
        ])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        let report = sense_integrity_tier0(
            &state,
            &lifecycle,
            &library_root,
            LaunchTier0RuntimeSelection::PreferredManaged,
        )
        .expect("report");

        assert_eq!(report.metadata_lookup_count, 2);
        assert_eq!(
            report.facts.len(),
            2,
            "outside files must never look healthy"
        );
        assert!(report.facts.iter().any(|fact| {
            fact.fields
                .iter()
                .any(|field| field.key == "observation" && field.value == "metadata_unavailable")
        }));
        assert!(report.facts.iter().any(|fact| {
            fact.fields
                .iter()
                .any(|field| field.key == "observation" && field.value == "wrong_type")
        }));
        drop(lifecycle);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    #[ignore = "requires AXIAL_I8_ROTATIONAL_FIXTURE_ROOT and AXIAL_I8_DEVICE_EVIDENCE"]
    async fn rotational_fixture_integrity_tier_zero_p95_is_within_declared_ceiling() {
        let fixture_root = std::env::var_os("AXIAL_I8_ROTATIONAL_FIXTURE_ROOT")
            .map(PathBuf::from)
            .expect("AXIAL_I8_ROTATIONAL_FIXTURE_ROOT is required");
        let device_evidence = std::env::var("AXIAL_I8_DEVICE_EVIDENCE")
            .expect("AXIAL_I8_DEVICE_EVIDENCE is required");
        let filesystem_evidence = std::env::var("AXIAL_I8_FILESYSTEM_EVIDENCE")
            .expect("AXIAL_I8_FILESYSTEM_EVIDENCE is required");
        let cache_evidence =
            std::env::var("AXIAL_I8_CACHE_EVIDENCE").expect("AXIAL_I8_CACHE_EVIDENCE is required");
        let cold_candidate_evidence = std::env::var("AXIAL_I8_COLD_CANDIDATE_EVIDENCE")
            .expect("AXIAL_I8_COLD_CANDIDATE_EVIDENCE is required");
        let entry_count = std::env::var("AXIAL_I8_FIXTURE_ENTRY_COUNT")
            .expect("AXIAL_I8_FIXTURE_ENTRY_COUNT is required")
            .parse::<usize>()
            .expect("AXIAL_I8_FIXTURE_ENTRY_COUNT must be an integer");
        assert!(
            entry_count >= 128,
            "I8 fixture must contain at least 128 entries"
        );
        let library_root = fs::canonicalize(&fixture_root).expect("canonical fixture root");
        let entries = (0..entry_count)
            .map(|index| {
                let relative = format!("benchmark/{index:05}.bin");
                let size = fs::symlink_metadata(library_root.join("libraries").join(&relative))
                    .expect("fixture entry metadata")
                    .len();
                entry(
                    TestKnownGoodRoot::Libraries,
                    &relative,
                    KnownGoodArtifactKind::Library,
                    TestKnownGoodIntegrity::File { size },
                )
            })
            .collect::<Vec<_>>();
        let (state, root) = state_fixture("i8", Some(library_root.clone()));
        let instance = state
            .instances()
            .insert_for_test("I8", "1.21.5")
            .expect("instance");
        state.activate_known_good_inventory_for_test(
            &instance.id,
            KnownGoodInventory::from_test_entries(entries).expect("I8 inventory"),
        );
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        let warmup_report = sense_integrity_tier0(
            &state,
            &lifecycle,
            &library_root,
            LaunchTier0RuntimeSelection::PreferredManaged,
        )
        .expect("warmup sensing");
        assert!(warmup_report.facts.is_empty(), "I8 fixture must be healthy");

        let mut samples = Vec::with_capacity(101);
        for _ in 0..101 {
            let started_at = Instant::now();
            let report = sense_integrity_tier0(
                &state,
                &lifecycle,
                &library_root,
                LaunchTier0RuntimeSelection::PreferredManaged,
            )
            .expect("sample sensing");
            samples.push(started_at.elapsed());
            assert!(
                report.facts.is_empty(),
                "I8 fixture drifted during measurement"
            );
            assert_eq!(report.metadata_lookup_count, entry_count);
        }
        samples.sort_unstable();
        let p50 = samples[50];
        let p95 = samples[95];
        let max = samples[100];
        println!(
            "{}",
            serde_json::json!({
                "schema": "axial.guardian.i8.integrity-tier0.v1",
                "fixture_root_supplied": true,
                "device_evidence": device_evidence,
                "filesystem_evidence": filesystem_evidence,
                "cache_evidence": cache_evidence,
                "cold_candidate_evidence": cold_candidate_evidence,
                "setup_metadata_reads_before_measurement": entry_count,
                "entry_count": entry_count,
                "warmup_samples": 1,
                "hot_samples": 101,
                "p50_micros": p50.as_micros(),
                "p95_micros": p95.as_micros(),
                "max_micros": max.as_micros(),
                "ceiling_ms": INTEGRITY_TIER0_CEILING_MS,
                "measurement_status": "candidate_only_pending_review"
            })
        );
        assert!(p95 <= Duration::from_millis(INTEGRITY_TIER0_CEILING_MS));
        drop(lifecycle);
        close_fixture(state, root).await;
    }
}
