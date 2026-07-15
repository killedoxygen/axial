//! Bounded integrity verification of exact live launcher-owned inventory authority.

use super::{
    ExecutionFact, ExecutionFactKind,
    low_priority::{
        LowPriorityOutcome, LowPriorityPlatform, SystemLowPriorityPlatform, run_at_low_priority,
    },
};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind};
use crate::state::{
    AppState, IdleSweepCancellation, IdleSweepSettlement, IdleSweepSettlementOwner,
    IdleSweepTerminal, InstanceLifecycleLease, IntegrityForegroundLease, KnownGoodTier2Ticket,
    KnownGoodVerificationLease, KnownGoodVerificationUnavailable, RegisteredArtifactCondition,
    RegisteredArtifactFindings, RegisteredArtifactObservation,
};
use axial_minecraft::ManagedRuntimeCache;
use axial_minecraft::known_good::{
    KnownGoodArtifactKind, KnownGoodEntry, KnownGoodIntegrity, KnownGoodInventory,
    KnownGoodPhysicalPath, KnownGoodRoot, LaunchTier0RuntimeSelection, LaunchTier1AdmittedFile,
    MAX_LAUNCH_TIER1_AGGREGATE_BYTES, Tier2Projection, known_good_entry_path,
    known_good_link_target_matches,
};
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Read};
#[cfg(any(windows, test))]
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const MAX_INTEGRITY_TIER0_FACTS: usize = 64;
#[cfg(any(windows, test))]
const WINDOWS_TIER0_WORKERS: usize = 4;

#[cfg(any(windows, test))]
fn tier0_worker_ranges(item_count: usize) -> Vec<Range<usize>> {
    let worker_count = item_count.min(WINDOWS_TIER0_WORKERS);
    (0..worker_count)
        .map(|worker| item_count * worker / worker_count..item_count * (worker + 1) / worker_count)
        .collect()
}

#[cfg(any(windows, test))]
fn order_indexed_results<T>(
    item_count: usize,
    results: impl IntoIterator<Item = (usize, T)>,
) -> Result<Vec<T>, ()> {
    let mut ordered = std::iter::repeat_with(|| None)
        .take(item_count)
        .collect::<Vec<_>>();
    for (index, result) in results {
        let Some(slot) = ordered.get_mut(index) else {
            return Err(());
        };
        if slot.replace(result).is_some() {
            return Err(());
        }
    }
    ordered.into_iter().collect::<Option<Vec<_>>>().ok_or(())
}

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
    Cancelled,
}

struct ContentHashResult {
    observation: io::Result<ContentHashObservation>,
    bytes_read: u64,
}

trait ContentReadControl {
    fn before_read(&mut self, next_read_bytes: usize) -> bool;
}

struct UnrestrictedContentReadControl;

impl ContentReadControl for UnrestrictedContentReadControl {
    fn before_read(&mut self, _next_read_bytes: usize) -> bool {
        true
    }
}

fn read_exact_sha1_controlled(
    reader: &mut impl Read,
    expected_size: u64,
    byte_budget: u64,
    control: &mut dyn ContentReadControl,
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
        if !control.before_read(limit) {
            return ContentHashResult {
                observation: Ok(ContentHashObservation::Cancelled),
                bytes_read,
            };
        }
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

#[cfg(test)]
fn read_exact_sha1(
    reader: &mut impl Read,
    expected_size: u64,
    byte_budget: u64,
) -> ContentHashResult {
    read_exact_sha1_controlled(
        reader,
        expected_size,
        byte_budget,
        &mut UnrestrictedContentReadControl,
    )
}

trait MetadataReader {
    fn symlink_metadata(&self, path: &KnownGoodPhysicalPath) -> io::Result<MetadataObservation>;
    fn tier0_metadata_batch(
        &self,
        paths: &[&KnownGoodPhysicalPath],
    ) -> Vec<io::Result<MetadataObservation>> {
        paths
            .iter()
            .map(|path| self.symlink_metadata(path))
            .collect()
    }
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

    fn hash_file_controlled(
        &self,
        path: &KnownGoodPhysicalPath,
        expected_size: u64,
        byte_budget: u64,
        control: &mut dyn ContentReadControl,
    ) -> ContentHashResult {
        if !control.before_read(0) {
            return ContentHashResult {
                observation: Ok(ContentHashObservation::Cancelled),
                bytes_read: 0,
            };
        }
        self.hash_file(path, expected_size, byte_budget)
    }

    fn revalidate(&self) -> io::Result<()>;
}

#[cfg(unix)]
mod confined_fs {
    use super::{
        ContentHashObservation, ContentHashResult, ContentReadControl, MetadataKind,
        MetadataObservation, read_exact_sha1_controlled,
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
        metadata_leaves: RefCell<Vec<HeldMetadataLeaf>>,
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

    struct HeldMetadataLeaf {
        parent: Rc<OwnedFd>,
        name: OsString,
        device: u64,
        inode: u64,
        mode: u32,
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
            self.metadata_leaves.borrow_mut().push(HeldMetadataLeaf {
                parent,
                name: leaf,
                device: stat.st_dev,
                inode: stat.st_ino,
                mode: stat.st_mode,
                size: stat.st_size,
                modified_seconds: stat.st_mtime,
                modified_nanoseconds: stat.st_mtime_nsec,
                changed_seconds: stat.st_ctime,
                changed_nanoseconds: stat.st_ctime_nsec,
            });
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
            control: &mut dyn ContentReadControl,
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
                let result =
                    read_exact_sha1_controlled(&mut readable, expected_size, byte_budget, control);
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
            for leaf in self.metadata_leaves.borrow().iter() {
                let current =
                    rustix::fs::statat(leaf.parent.as_ref(), &leaf.name, AtFlags::SYMLINK_NOFOLLOW)
                        .map_err(io::Error::from)?;
                if current.st_dev != leaf.device
                    || current.st_ino != leaf.inode
                    || current.st_mode != leaf.mode
                    || current.st_size != leaf.size
                    || current.st_mtime != leaf.modified_seconds
                    || current.st_mtime_nsec != leaf.modified_nanoseconds
                    || current.st_ctime != leaf.changed_seconds
                    || current.st_ctime_nsec != leaf.changed_nanoseconds
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good metadata leaf changed after observation",
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
        ContentHashObservation, ContentHashResult, ContentReadControl, MetadataKind,
        MetadataObservation, order_indexed_results, read_exact_sha1_controlled,
        tier0_worker_ranges,
    };
    use axial_minecraft::known_good::{KnownGoodPhysicalPath, MAX_LAUNCH_TIER0_ENTRIES};
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
    use std::thread;
    use std::time::SystemTime;
    use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
    use windows_sys::Wdk::Storage::FileSystem::{
        FILE_DIRECTORY_FILE, FILE_NETWORK_OPEN_INFORMATION, FILE_NON_DIRECTORY_FILE, FILE_OPEN,
        FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT, FileNetworkOpenInformation,
        NtCreateFile, NtQueryInformationFile,
    };
    use windows_sys::Win32::Foundation::{
        CloseHandle, GENERIC_READ, HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError,
        UNICODE_STRING,
    };
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO, FILE_EXECUTE,
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
        tier0_metadata_handles: RefCell<Vec<fs::File>>,
        revalidated_metadata_leaves: RefCell<Vec<HeldRevalidatedMetadataLeaf>>,
    }

    struct PreparedTier0Leaf {
        input_index: usize,
        parent: Rc<fs::File>,
        name: OsString,
    }

    #[derive(Clone, Copy)]
    struct Tier0WorkerLeaf<'a> {
        batch_index: usize,
        input_index: usize,
        parent: &'a fs::File,
        name: &'a OsStr,
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

    struct HeldRevalidatedMetadataLeaf {
        parent: Rc<fs::File>,
        name: OsString,
        file: Rc<fs::File>,
        volume_serial_number: u64,
        file_id: [u8; 16],
        attributes: u32,
        size: i64,
        modified: i64,
        changed: i64,
    }

    fn tier0_observation(info: FILE_NETWORK_OPEN_INFORMATION) -> MetadataObservation {
        let kind = if info.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            MetadataKind::Link
        } else if info.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 {
            MetadataKind::Directory
        } else {
            MetadataKind::File
        };
        MetadataObservation {
            kind,
            size: info.EndOfFile.try_into().unwrap_or_default(),
            modified: (info.LastWriteTime != 0).then_some(SystemTime::UNIX_EPOCH),
        }
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
            Self::open_relative_with_access_and_share(
                parent,
                name,
                directory,
                access,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
            )
        }

        fn open_relative_with_access_and_share(
            parent: &fs::File,
            name: &OsStr,
            directory: Option<bool>,
            access: u32,
            share: u32,
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
                    share,
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
            let file = Rc::new(Self::open_relative(parent.as_ref(), &leaf, None)?);
            let basic: FILE_BASIC_INFO = Self::query(file.as_ref(), FileBasicInfo)?;
            let standard: FILE_STANDARD_INFO = Self::query(file.as_ref(), FileStandardInfo)?;
            let id: FILE_ID_INFO = Self::query(file.as_ref(), FileIdInfo)?;
            self.revalidated_metadata_leaves
                .borrow_mut()
                .push(HeldRevalidatedMetadataLeaf {
                    parent,
                    name: leaf,
                    file,
                    volume_serial_number: id.VolumeSerialNumber,
                    file_id: id.FileId.Identifier,
                    attributes: basic.FileAttributes,
                    size: standard.EndOfFile,
                    modified: basic.LastWriteTime,
                    changed: basic.ChangeTime,
                });
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

        fn open_and_query_tier0(
            parent: &fs::File,
            name: &OsStr,
        ) -> io::Result<(MetadataObservation, fs::File)> {
            let file = Self::open_relative_with_access_and_share(
                parent,
                name,
                None,
                FILE_EXECUTE,
                FILE_SHARE_READ,
            )?;
            let mut info = FILE_NETWORK_OPEN_INFORMATION::default();
            let mut status = IO_STATUS_BLOCK::default();
            let result = unsafe {
                NtQueryInformationFile(
                    file.as_raw_handle() as HANDLE,
                    &mut status,
                    (&mut info as *mut FILE_NETWORK_OPEN_INFORMATION).cast(),
                    size_of::<FILE_NETWORK_OPEN_INFORMATION>() as u32,
                    FileNetworkOpenInformation,
                )
            };
            if result < 0 {
                let code = unsafe { RtlNtStatusToDosError(result) };
                return Err(io::Error::from_raw_os_error(code as i32));
            }
            // FILE_EXECUTE engages read-class share enforcement; no read or map call is made.
            // Last-write time is only an availability counter, not integrity authority.
            Ok((tier0_observation(info), file))
        }

        pub(super) fn tier0_metadata_batch(
            &self,
            paths: &[&KnownGoodPhysicalPath],
        ) -> Vec<io::Result<MetadataObservation>> {
            if paths.len() > MAX_LAUNCH_TIER0_ENTRIES {
                return paths
                    .iter()
                    .map(|_| {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Tier 0 metadata batch exceeds its entry bound",
                        ))
                    })
                    .collect();
            }

            let mut results = std::iter::repeat_with(|| None)
                .take(paths.len())
                .collect::<Vec<_>>();
            let mut prepared = Vec::with_capacity(paths.len());
            for (input_index, path) in paths.iter().enumerate() {
                match self.parent(path) {
                    Ok((parent, name)) => prepared.push(PreparedTier0Leaf {
                        input_index,
                        parent,
                        name,
                    }),
                    Err(error) => results[input_index] = Some(Err(error)),
                }
            }

            let worker_jobs = prepared
                .iter()
                .enumerate()
                .map(|(batch_index, prepared)| Tier0WorkerLeaf {
                    batch_index,
                    input_index: prepared.input_index,
                    parent: prepared.parent.as_ref(),
                    name: prepared.name.as_os_str(),
                })
                .collect::<Vec<_>>();
            let indexed_results = thread::scope(|scope| {
                let mut indexed_results = Vec::with_capacity(worker_jobs.len());
                let mut workers = Vec::new();
                for (worker_index, range) in tier0_worker_ranges(worker_jobs.len())
                    .into_iter()
                    .enumerate()
                {
                    let worker_range = range.clone();
                    let jobs = &worker_jobs[range];
                    match thread::Builder::new()
                        .name(format!("axial-tier0-open-{worker_index}"))
                        .spawn_scoped(scope, move || {
                            jobs.iter()
                                .map(|job| {
                                    (
                                        job.batch_index,
                                        (
                                            job.input_index,
                                            Self::open_and_query_tier0(job.parent, job.name),
                                        ),
                                    )
                                })
                                .collect::<Vec<_>>()
                        }) {
                        Ok(worker) => workers.push((worker_range, worker)),
                        Err(error) => {
                            let kind = error.kind();
                            indexed_results.extend(worker_jobs[worker_range].iter().map(|job| {
                                (
                                    job.batch_index,
                                    (
                                        job.input_index,
                                        Err(io::Error::new(
                                            kind,
                                            "failed to spawn Tier 0 metadata worker",
                                        )),
                                    ),
                                )
                            }));
                        }
                    }
                }
                for (range, worker) in workers {
                    match worker.join() {
                        Ok(worker_results) => indexed_results.extend(worker_results),
                        Err(_) => {
                            indexed_results.extend(worker_jobs[range].iter().map(|job| {
                                (
                                    job.batch_index,
                                    (
                                        job.input_index,
                                        Err(io::Error::other("Tier 0 metadata worker failed")),
                                    ),
                                )
                            }));
                        }
                    }
                }
                indexed_results
            });

            match order_indexed_results(worker_jobs.len(), indexed_results) {
                Ok(ordered) => {
                    for (input_index, result) in ordered {
                        results[input_index] = Some(result);
                    }
                }
                Err(()) => {
                    for prepared in &prepared {
                        results[prepared.input_index] = Some(Err(io::Error::other(
                            "Tier 0 metadata worker returned malformed results",
                        )));
                    }
                }
            }

            let mut retained = self.tier0_metadata_handles.borrow_mut();
            results
                .into_iter()
                .map(|result| {
                    match result.unwrap_or_else(|| {
                        Err(io::Error::other("Tier 0 metadata result is missing"))
                    }) {
                        Ok((observation, file)) => {
                            retained.push(file);
                            Ok(observation)
                        }
                        Err(error) => Err(error),
                    }
                })
                .collect()
        }

        pub(super) fn finish_tier0(&self) {
            let retained = self.tier0_metadata_handles.take();
            if retained.is_empty() {
                return;
            }
            let ranges = tier0_worker_ranges(retained.len());
            let mut retained = retained.into_iter();
            let mut workers = Vec::with_capacity(ranges.len());
            for (worker_index, range) in ranges.into_iter().enumerate() {
                let bucket = retained.by_ref().take(range.len()).collect::<Vec<_>>();
                if let Ok(worker) = thread::Builder::new()
                    .name(format!("axial-tier0-close-{worker_index}"))
                    .spawn(move || drop(bucket))
                {
                    workers.push(worker);
                }
                // On spawn failure the rejected closure and its bucket are dropped here.
            }
            for worker in workers {
                let _ = worker.join();
            }
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
            control: &mut dyn ContentReadControl,
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
                let result =
                    read_exact_sha1_controlled(&mut readable, expected_size, byte_budget, control);
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
            for leaf in self.revalidated_metadata_leaves.borrow().iter() {
                let held_basic: FILE_BASIC_INFO = Self::query(leaf.file.as_ref(), FileBasicInfo)?;
                let held_standard: FILE_STANDARD_INFO =
                    Self::query(leaf.file.as_ref(), FileStandardInfo)?;
                let held_id: FILE_ID_INFO = Self::query(leaf.file.as_ref(), FileIdInfo)?;
                if held_id.VolumeSerialNumber != leaf.volume_serial_number
                    || held_id.FileId.Identifier != leaf.file_id
                    || held_basic.FileAttributes != leaf.attributes
                    || held_standard.EndOfFile != leaf.size
                    || held_basic.LastWriteTime != leaf.modified
                    || held_basic.ChangeTime != leaf.changed
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good metadata leaf changed after observation",
                    ));
                }
                let current = Self::open_relative(leaf.parent.as_ref(), &leaf.name, None)?;
                let current_id: FILE_ID_INFO = Self::query(&current, FileIdInfo)?;
                if current_id.VolumeSerialNumber != leaf.volume_serial_number
                    || current_id.FileId.Identifier != leaf.file_id
                {
                    return Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "known-good metadata leaf identity changed",
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

    #[cfg(test)]
    mod tests {
        use super::*;
        use std::io::Read;
        use std::time::{SystemTime, UNIX_EPOCH};
        use windows_sys::Wdk::Storage::FileSystem::{
            FILE_RENAME_POSIX_SEMANTICS, FILE_RENAME_REPLACE_IF_EXISTS,
        };
        use windows_sys::Win32::Foundation::ERROR_SHARING_VIOLATION;
        use windows_sys::Win32::Storage::FileSystem::{
            DELETE, FILE_RENAME_INFO, FILE_SHARE_DELETE, FileRenameInfoEx,
            SetFileInformationByHandle,
        };

        fn test_root(label: &str) -> PathBuf {
            std::env::temp_dir().join(format!(
                "axial-windows-tier0-{label}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .expect("clock")
                    .as_nanos()
            ))
        }

        fn replace_with_posix_semantics(source: &fs::File, target: &Path) -> io::Result<()> {
            if !target.is_absolute() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rename target must be absolute",
                ));
            }
            let target_name: Vec<u16> = target.as_os_str().encode_wide().collect();
            if target_name.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "rename target must not be empty",
                ));
            }
            let name_bytes = target_name
                .len()
                .checked_mul(size_of::<u16>())
                .ok_or_else(|| io::Error::other("rename target is too long"))?;
            let buffer_size = size_of::<FILE_RENAME_INFO>()
                .checked_add(name_bytes)
                .ok_or_else(|| io::Error::other("rename buffer size overflow"))?;
            let word_count = buffer_size.div_ceil(size_of::<usize>());
            let mut buffer = vec![0_usize; word_count];
            let info = buffer.as_mut_ptr().cast::<FILE_RENAME_INFO>();
            unsafe {
                (*info).Anonymous.Flags =
                    FILE_RENAME_REPLACE_IF_EXISTS | FILE_RENAME_POSIX_SEMANTICS;
                (*info).RootDirectory = ptr::null_mut();
                (*info).FileNameLength = name_bytes
                    .try_into()
                    .map_err(|_| io::Error::other("rename target is too long"))?;
                ptr::copy_nonoverlapping(
                    target_name.as_ptr(),
                    (*info).FileName.as_mut_ptr(),
                    target_name.len(),
                );
                if SetFileInformationByHandle(
                    source.as_raw_handle() as HANDLE,
                    FileRenameInfoEx,
                    info.cast(),
                    buffer_size
                        .try_into()
                        .map_err(|_| io::Error::other("rename buffer is too large"))?,
                ) == 0
                {
                    return Err(io::Error::last_os_error());
                }
            }
            Ok(())
        }

        fn tier0_one(
            reader: &Reader,
            path: &KnownGoodPhysicalPath,
        ) -> io::Result<MetadataObservation> {
            let mut results = reader.tier0_metadata_batch(&[path]);
            assert_eq!(results.len(), 1);
            results.pop().expect("one Tier 0 result")
        }

        #[test]
        fn tier0_metadata_handle_rejects_posix_namespace_replacement() {
            let root = test_root("retained-handle");
            let parent = root.join("libraries");
            fs::create_dir_all(&parent).expect("fixture parent");
            let leaf = parent.join("library.jar");
            let replacement = parent.join("replacement.jar");
            fs::write(&leaf, b"1234567").expect("fixture leaf");
            fs::write(&replacement, b"7654321").expect("replacement leaf");
            let path = KnownGoodPhysicalPath::for_test(
                root.clone(),
                PathBuf::from("libraries/library.jar"),
            );

            let reader = Reader::default();
            let observation = tier0_one(&reader, &path).expect("Tier 0 metadata");
            assert_eq!(observation.kind, MetadataKind::File);
            assert_eq!(observation.size, 7);
            assert_eq!(reader.tier0_metadata_handles.borrow().len(), 1);
            assert!(reader.revalidated_metadata_leaves.borrow().is_empty());
            reader.revalidate().expect("root currency");

            assert!(
                fs::OpenOptions::new().write(true).open(&leaf).is_err(),
                "retained Tier 0 handle must reject data writers"
            );
            let replacement_handle = fs::OpenOptions::new()
                .access_mode(DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&replacement)
                .expect("replacement handle");
            let error = replace_with_posix_semantics(&replacement_handle, &leaf)
                .expect_err("retained Tier 0 handle must reject POSIX replacement");
            assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));
            drop(replacement_handle);

            assert_eq!(fs::read(&leaf).expect("original contents"), b"1234567");
            reader.revalidate().expect("root currency after rejection");

            reader.finish_tier0();
            assert!(reader.tier0_metadata_handles.borrow().is_empty());
            drop(
                fs::OpenOptions::new()
                    .write(true)
                    .open(&leaf)
                    .expect("writer after joined close"),
            );
            let replacement_handle = fs::OpenOptions::new()
                .access_mode(DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&replacement)
                .expect("replacement handle after close");
            replace_with_posix_semantics(&replacement_handle, &leaf)
                .expect("replacement after joined close");
            drop(replacement_handle);
            assert_eq!(fs::read(&leaf).expect("replacement contents"), b"7654321");

            let _ = fs::remove_dir_all(root);
        }

        #[test]
        fn posix_replacement_succeeds_when_target_handle_shares_delete() {
            let root = test_root("delete-sharing-control");
            fs::create_dir_all(&root).expect("fixture root");
            let target = root.join("target.jar");
            let replacement = root.join("replacement.jar");
            fs::write(&target, b"old bytes").expect("target leaf");
            fs::write(&replacement, b"new bytes").expect("replacement leaf");

            let mut target_handle = fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&target)
                .expect("target handle");
            let replacement_handle = fs::OpenOptions::new()
                .access_mode(DELETE)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&replacement)
                .expect("replacement handle");

            replace_with_posix_semantics(&replacement_handle, &target)
                .expect("delete-sharing target permits POSIX replacement");
            drop(replacement_handle);

            let mut held_bytes = Vec::new();
            target_handle
                .read_to_end(&mut held_bytes)
                .expect("read held target");
            assert_eq!(held_bytes, b"old bytes");
            assert_eq!(fs::read(&target).expect("reopened target"), b"new bytes");

            drop(target_handle);
            let _ = fs::remove_dir_all(root);
        }

        #[test]
        fn tier0_metadata_fails_closed_when_writer_is_already_open() {
            let root = test_root("existing-writer");
            let parent = root.join("libraries");
            fs::create_dir_all(&parent).expect("fixture parent");
            let leaf = parent.join("library.jar");
            fs::write(&leaf, b"1234567").expect("fixture leaf");
            let writer = fs::OpenOptions::new()
                .write(true)
                .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
                .open(&leaf)
                .expect("existing writer");
            let path = KnownGoodPhysicalPath::for_test(
                root.clone(),
                PathBuf::from("libraries/library.jar"),
            );

            let reader = Reader::default();
            let error =
                tier0_one(&reader, &path).expect_err("Tier 0 must reject an existing data writer");
            assert_eq!(error.raw_os_error(), Some(ERROR_SHARING_VIOLATION as i32));

            drop(writer);
            drop(reader);
            let _ = fs::remove_dir_all(root);
        }

        #[test]
        fn tier0_batch_preserves_input_order_and_retains_every_successful_leaf() {
            let root = test_root("ordered-batch");
            let first_parent = root.join("libraries/first");
            let second_parent = root.join("libraries/second");
            fs::create_dir_all(&first_parent).expect("first parent");
            fs::create_dir_all(&second_parent).expect("second parent");
            let mut relative_paths = Vec::new();
            for index in 0..8 {
                let parent = if index % 2 == 0 {
                    &first_parent
                } else {
                    &second_parent
                };
                let leaf = parent.join(format!("{index}.jar"));
                fs::write(&leaf, vec![b'x'; index + 1]).expect("fixture leaf");
                relative_paths.push(PathBuf::from(format!(
                    "libraries/{}/{index}.jar",
                    if index % 2 == 0 { "first" } else { "second" }
                )));
            }
            relative_paths.reverse();
            let paths = relative_paths
                .iter()
                .cloned()
                .map(|relative| KnownGoodPhysicalPath::for_test(root.clone(), relative))
                .collect::<Vec<_>>();
            let path_refs = paths.iter().collect::<Vec<_>>();

            let reader = Reader::default();
            let results = reader.tier0_metadata_batch(&path_refs);

            assert_eq!(results.len(), paths.len());
            for (result, path) in results.into_iter().zip(&paths) {
                let observation = result.expect("batch observation");
                let index = path
                    .relative()
                    .file_stem()
                    .and_then(OsStr::to_str)
                    .and_then(|value| value.parse::<usize>().ok())
                    .expect("fixture index");
                assert_eq!(observation.kind, MetadataKind::File);
                assert_eq!(observation.size, (index + 1) as u64);
            }
            assert_eq!(reader.tier0_metadata_handles.borrow().len(), paths.len());
            for path in &paths {
                assert!(
                    fs::OpenOptions::new()
                        .write(true)
                        .open(path.root().join(path.relative()))
                        .is_err(),
                    "every retained leaf must reject writers"
                );
            }

            reader.finish_tier0();
            assert!(reader.tier0_metadata_handles.borrow().is_empty());
            for path in &paths {
                drop(
                    fs::OpenOptions::new()
                        .write(true)
                        .open(path.root().join(path.relative()))
                        .expect("writer after joined close"),
                );
            }

            let _ = fs::remove_dir_all(root);
        }

        #[test]
        fn tier0_network_observation_preserves_reparse_precedence_and_exact_size() {
            let observation = tier0_observation(FILE_NETWORK_OPEN_INFORMATION {
                EndOfFile: 37,
                FileAttributes: FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY,
                LastWriteTime: 1,
                ..FILE_NETWORK_OPEN_INFORMATION::default()
            });

            assert_eq!(observation.kind, MetadataKind::Link);
            assert_eq!(observation.size, 37);
            assert!(observation.modified.is_some());
        }
    }
}

#[derive(Default)]
struct FilesystemIntegrityReader {
    #[cfg(any(unix, windows))]
    inner: confined_fs::Reader,
}

impl MetadataReader for FilesystemIntegrityReader {
    fn symlink_metadata(&self, path: &KnownGoodPhysicalPath) -> io::Result<MetadataObservation> {
        #[cfg(any(unix, windows))]
        return self.inner.metadata(path);
        #[cfg(not(any(unix, windows)))]
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "race-resistant known-good metadata is unavailable on this platform",
        ))
    }

    #[cfg(windows)]
    fn tier0_metadata_batch(
        &self,
        paths: &[&KnownGoodPhysicalPath],
    ) -> Vec<io::Result<MetadataObservation>> {
        self.inner.tier0_metadata_batch(paths)
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

impl FilesystemIntegrityReader {
    fn finish_tier0(&self) {
        #[cfg(windows)]
        self.inner.finish_tier0();
    }
}

impl ContentReader for FilesystemIntegrityReader {
    fn hash_file(
        &self,
        path: &KnownGoodPhysicalPath,
        expected_size: u64,
        byte_budget: u64,
    ) -> ContentHashResult {
        self.hash_file_controlled(
            path,
            expected_size,
            byte_budget,
            &mut UnrestrictedContentReadControl,
        )
    }

    fn hash_file_controlled(
        &self,
        path: &KnownGoodPhysicalPath,
        expected_size: u64,
        byte_budget: u64,
        control: &mut dyn ContentReadControl,
    ) -> ContentHashResult {
        #[cfg(any(unix, windows))]
        return self
            .inner
            .hash_file(path, expected_size, byte_budget, control);
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
    foreground: &IntegrityForegroundLease,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
    runtime_selection: LaunchTier0RuntimeSelection<'_>,
) -> Result<IntegrityTier0Report, KnownGoodVerificationUnavailable> {
    let lease =
        state.mint_known_good_verification_lease(foreground, lifecycle, expected_library_root)?;
    let reader = FilesystemIntegrityReader::default();
    let report = sense_integrity_tier0_with(&lease, runtime_selection, &reader);
    let lease_is_current = state.known_good_verification_lease_can_admit(&lease);
    reader.finish_tier0();
    if !lease_is_current {
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
    let jobs = projection
        .into_iter()
        .map(|(ordinal, entry)| {
            (
                ordinal,
                entry,
                known_good_entry_path(library_root, managed_runtime_cache, entry),
            )
        })
        .collect::<Vec<_>>();
    report.metadata_lookup_count = jobs.len();
    let paths = jobs.iter().map(|(_, _, path)| path).collect::<Vec<_>>();
    let observations = reader.tier0_metadata_batch(&paths);
    let mut sensed_facts = Vec::new();
    if observations.len() != jobs.len() {
        sensed_facts.extend(jobs.iter().map(|(ordinal, entry, _)| {
            integrity_fact(
                entry,
                *ordinal,
                ExecutionFactKind::PrimitiveRefused,
                "metadata_unavailable",
            )
        }));
    } else {
        for ((ordinal, entry, path), observation) in jobs.iter().zip(observations) {
            let fact = match observation {
                Ok(observation) => {
                    report.mtime_observation_count += usize::from(observation.modified.is_some());
                    inspect_observation(reader, entry, path, *ordinal, observation, &mut report)
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Some(integrity_fact(
                    entry,
                    *ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "missing",
                )),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    Some(integrity_fact(
                        entry,
                        *ordinal,
                        ExecutionFactKind::FilePermissionDenied,
                        "metadata_permission_denied",
                    ))
                }
                Err(_) => Some(integrity_fact(
                    entry,
                    *ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "metadata_unavailable",
                )),
            };
            if let Some(fact) = fact {
                sensed_facts.push(fact);
            }
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
    repairable: bool,
}

#[derive(Debug)]
struct Tier1RepairableObservation {
    fact_index: usize,
    observation: RegisteredArtifactObservation,
}

#[derive(Debug, Default)]
pub(crate) struct IntegrityTier1Report {
    pub(crate) facts: Vec<ExecutionFact>,
    pub(crate) hashed_entry_count: usize,
    pub(crate) content_read_byte_count: u64,
    pub(crate) suppressed_fact_count: usize,
    repairable_observations: Vec<Tier1RepairableObservation>,
}

pub(crate) struct AdmittedIntegrityTier1Report {
    report: IntegrityTier1Report,
    findings: RegisteredArtifactFindings,
}

impl std::fmt::Debug for AdmittedIntegrityTier1Report {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AdmittedIntegrityTier1Report")
            .field("report", &self.report)
            .field("finding_count", &self.findings.len())
            .finish()
    }
}

impl AdmittedIntegrityTier1Report {
    #[cfg(test)]
    pub(crate) fn findings(&self) -> &RegisteredArtifactFindings {
        &self.findings
    }

    pub(crate) fn into_parts(self) -> (IntegrityTier1Report, RegisteredArtifactFindings) {
        (self.report, self.findings)
    }
}

impl std::ops::Deref for AdmittedIntegrityTier1Report {
    type Target = IntegrityTier1Report;

    fn deref(&self) -> &Self::Target {
        &self.report
    }
}

pub(crate) async fn sense_integrity_tier1(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
) -> Result<AdmittedIntegrityTier1Report, KnownGoodVerificationUnavailable> {
    let lease =
        state.mint_known_good_verification_lease(foreground, lifecycle, expected_library_root)?;
    sense_integrity_tier1_with_lease(state, lease, FilesystemIntegrityReader::default).await
}

pub(crate) async fn sense_current_integrity_tier1(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    lifecycle: &InstanceLifecycleLease,
) -> Result<AdmittedIntegrityTier1Report, KnownGoodVerificationUnavailable> {
    let lease = state.mint_current_known_good_verification_lease(foreground, lifecycle)?;
    sense_integrity_tier1_with_lease(state, lease, FilesystemIntegrityReader::default).await
}

#[cfg(test)]
async fn sense_integrity_tier1_with_reader_factory<Factory, Reader>(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    lifecycle: &InstanceLifecycleLease,
    expected_library_root: &Path,
    reader_factory: Factory,
) -> Result<AdmittedIntegrityTier1Report, KnownGoodVerificationUnavailable>
where
    Factory: FnOnce() -> Reader + Send + 'static,
    Reader: ContentReader,
{
    let lease =
        state.mint_known_good_verification_lease(foreground, lifecycle, expected_library_root)?;
    sense_integrity_tier1_with_lease(state, lease, reader_factory).await
}

async fn sense_integrity_tier1_with_lease<Factory, Reader>(
    state: &AppState,
    lease: KnownGoodVerificationLease,
    reader_factory: Factory,
) -> Result<AdmittedIntegrityTier1Report, KnownGoodVerificationUnavailable>
where
    Factory: FnOnce() -> Reader + Send + 'static,
    Reader: ContentReader,
{
    let prepared = prepare_tier1_jobs(&lease);
    let (lease, mut report) = match prepared {
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
    if !state.known_good_verification_lease_can_admit(&lease) {
        return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
    }
    let observations = report
        .repairable_observations
        .iter()
        .map(|observed| observed.observation)
        .collect();
    let findings = state.seal_registered_artifact_findings(lease, observations)?;
    if !report.bind_registered_artifact_targets(&findings) {
        return Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable);
    }
    Ok(AdmittedIntegrityTier1Report { report, findings })
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
        let repairable = lease
            .registered_artifact_observation(
                inventory_ordinal,
                RegisteredArtifactCondition::Corrupt,
            )
            .is_some();
        jobs.push(Tier1HashJob {
            path: file.physical_path(library_root),
            file,
            inventory_ordinal,
            repairable,
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
            sensed_facts.push((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "content_budget_exceeded",
                ),
                None,
            ));
            break;
        }
        let sensed = match result.observation {
            Ok(ContentHashObservation::Hashed { digest }) => {
                report.hashed_entry_count += 1;
                (digest != job.file.digest().as_str()).then(|| {
                    (
                        tier1_integrity_fact(
                            &job.file,
                            job.inventory_ordinal,
                            ExecutionFactKind::ArtifactHashMismatch,
                            "hash_mismatch",
                        ),
                        job.repairable.then(|| {
                            RegisteredArtifactObservation::new(
                                job.inventory_ordinal,
                                RegisteredArtifactCondition::Corrupt,
                            )
                        }),
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
                Some((
                    fact,
                    job.repairable.then(|| {
                        RegisteredArtifactObservation::new(
                            job.inventory_ordinal,
                            RegisteredArtifactCondition::Corrupt,
                        )
                    }),
                ))
            }
            Ok(ContentHashObservation::WrongType) => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "wrong_type",
                ),
                job.repairable.then(|| {
                    RegisteredArtifactObservation::new(
                        job.inventory_ordinal,
                        RegisteredArtifactCondition::Corrupt,
                    )
                }),
            )),
            Ok(ContentHashObservation::ChangedDuringRead) => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "content_changed_during_read",
                ),
                None,
            )),
            Ok(ContentHashObservation::BudgetRefused) => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "content_budget_refused",
                ),
                None,
            )),
            Ok(ContentHashObservation::Cancelled) => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "content_read_cancelled",
                ),
                None,
            )),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "missing",
                ),
                job.repairable.then(|| {
                    RegisteredArtifactObservation::new(
                        job.inventory_ordinal,
                        RegisteredArtifactCondition::Missing,
                    )
                }),
            )),
            Err(error) if error.kind() == io::ErrorKind::PermissionDenied => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::FilePermissionDenied,
                    "content_permission_denied",
                ),
                None,
            )),
            Err(_) => Some((
                tier1_integrity_fact(
                    &job.file,
                    job.inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "content_unavailable",
                ),
                None,
            )),
        };
        if let Some(sensed) = sensed {
            sensed_facts.push(sensed);
        }
    }
    if reader.revalidate().is_err() {
        push_bounded_tier1_fact(&mut report, tier1_confinement_refused_fact());
    } else {
        for (fact, observation) in sensed_facts {
            if let Some(fact_index) = push_bounded_tier1_fact(&mut report, fact)
                && let Some(observation) = observation
            {
                report
                    .repairable_observations
                    .push(Tier1RepairableObservation {
                        fact_index,
                        observation,
                    });
            }
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

fn push_bounded_tier1_fact(
    report: &mut IntegrityTier1Report,
    fact: ExecutionFact,
) -> Option<usize> {
    if report.facts.len() < MAX_INTEGRITY_TIER1_FACTS {
        let fact_index = report.facts.len();
        report.facts.push(fact);
        Some(fact_index)
    } else {
        report.suppressed_fact_count += 1;
        None
    }
}

impl IntegrityTier1Report {
    fn bind_registered_artifact_targets(&mut self, findings: &RegisteredArtifactFindings) -> bool {
        for observed in &self.repairable_observations {
            let Some(fact) = self.facts.get_mut(observed.fact_index) else {
                return false;
            };
            let Some(target) = findings.target_for(observed.observation) else {
                return false;
            };
            fact.target = Some(target.clone());
        }
        self.repairable_observations.clear();
        true
    }
}

const MAX_INTEGRITY_TIER2_FACTS: usize = 64;
const MAX_INTEGRITY_TIER2_BATCH_ENTRIES: usize = 128;
const INTEGRITY_TIER2_BATCH_CONTENT_THRESHOLD_BYTES: u64 = 64 << 20;
const INTEGRITY_TIER2_BYTES_PER_SECOND: u64 = 8 << 20;
const INTEGRITY_TIER2_ENTRIES_PER_SECOND: u64 = 64;
const INTEGRITY_TIER2_BYTE_BURST: u64 = 64 * 1024;
const MAX_INTEGRITY_TIER2_THROTTLE_SLEEP: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IntegrityTier2Status {
    Complete,
    Cancelled,
    Refused,
}

#[derive(Debug)]
pub(crate) struct IntegrityTier2Report {
    pub(crate) status: IntegrityTier2Status,
    pub(crate) facts: Vec<ExecutionFact>,
    pub(crate) selected_entry_count: usize,
    pub(crate) verified_entry_count: usize,
    pub(crate) processed_entry_count: usize,
    pub(crate) hashed_entry_count: usize,
    pub(crate) expected_content_byte_count: u64,
    pub(crate) content_read_byte_count: u64,
    pub(crate) metadata_lookup_count: usize,
    pub(crate) link_lookup_count: usize,
    pub(crate) suppressed_fact_count: usize,
    repairable_observations: Vec<Tier2RepairableObservation>,
}

#[derive(Debug)]
struct Tier2RepairableObservation {
    fact_index: usize,
    observation: RegisteredArtifactObservation,
}

#[must_use = "Tier 2 work must be run by its blocking owner"]
pub(crate) struct IntegrityTier2OwnedWork {
    state: AppState,
    ticket: KnownGoodTier2Ticket,
    settlement: IdleSweepSettlementOwner,
}

pub(crate) struct IntegrityTier2OwnedWorkAuthorityMismatch {
    ticket: KnownGoodTier2Ticket,
    settlement: IdleSweepSettlementOwner,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum IntegrityTier2OwnedWorkRejection {
    Cancelled,
    Refused,
}

impl std::fmt::Debug for IntegrityTier2OwnedWorkAuthorityMismatch {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntegrityTier2OwnedWorkAuthorityMismatch")
            .finish_non_exhaustive()
    }
}

impl IntegrityTier2OwnedWorkAuthorityMismatch {
    pub(crate) fn settle(self) -> IntegrityTier2OwnedWorkRejection {
        let Self { ticket, settlement } = self;
        drop(ticket);
        if settlement.is_current() {
            settlement.settle(IdleSweepTerminal::Refused);
            IntegrityTier2OwnedWorkRejection::Refused
        } else {
            settlement.settle(IdleSweepTerminal::Cancelled);
            IntegrityTier2OwnedWorkRejection::Cancelled
        }
    }
}

#[must_use = "Tier 2 completion retains the ticket and explicit sweep settlement owner"]
pub(crate) struct IntegrityTier2OwnedResult {
    state: AppState,
    report: IntegrityTier2Report,
    ticket: KnownGoodTier2Ticket,
    settlement: IdleSweepSettlementOwner,
}

impl std::fmt::Debug for IntegrityTier2OwnedResult {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IntegrityTier2OwnedResult")
            .field("report", &self.report)
            .field("sweep_ownership", &"retained")
            .finish()
    }
}

pub(crate) struct Tier2RegisteredArtifactSealRequest {
    ticket: KnownGoodTier2Ticket,
    observations: Vec<RegisteredArtifactObservation>,
}

impl Tier2RegisteredArtifactSealRequest {
    fn new(ticket: KnownGoodTier2Ticket, observations: Vec<RegisteredArtifactObservation>) -> Self {
        Self {
            ticket,
            observations,
        }
    }

    pub(crate) fn into_parts(self) -> (KnownGoodTier2Ticket, Vec<RegisteredArtifactObservation>) {
        (self.ticket, self.observations)
    }
}

struct SealedIntegrityTier2Result {
    report: IntegrityTier2Report,
    findings: Option<RegisteredArtifactFindings>,
    settlement: IdleSweepSettlementOwner,
}

impl IntegrityTier2OwnedResult {
    async fn seal_and_bind(self) -> SealedIntegrityTier2Result {
        let Self {
            state,
            mut report,
            ticket,
            settlement,
        } = self;
        if report.status != IntegrityTier2Status::Complete {
            drop(ticket);
            return SealedIntegrityTier2Result {
                report,
                findings: None,
                settlement,
            };
        }

        let request = Tier2RegisteredArtifactSealRequest::new(
            ticket,
            report.registered_artifact_observations(),
        );
        let findings = match state.seal_tier2_registered_artifact_request(request).await {
            Ok(findings) => findings,
            Err(_) if !settlement.is_current() => {
                return SealedIntegrityTier2Result {
                    report: report.cancel(),
                    findings: None,
                    settlement,
                };
            }
            Err(_) => {
                return SealedIntegrityTier2Result {
                    report: report.refuse(tier2_authority_refused_fact()),
                    findings: None,
                    settlement,
                };
            }
        };
        if !report.bind_registered_artifact_targets(&findings) {
            return SealedIntegrityTier2Result {
                report: report.refuse(tier2_authority_refused_fact()),
                findings: None,
                settlement,
            };
        }
        SealedIntegrityTier2Result {
            report,
            findings: Some(findings),
            settlement,
        }
    }

    pub(crate) async fn settle_without_recovery(
        self,
    ) -> (IntegrityTier2Report, IdleSweepSettlement) {
        self.seal_and_bind().await.settle_without_recovery()
    }
}

impl SealedIntegrityTier2Result {
    fn settle_without_recovery(self) -> (IntegrityTier2Report, IdleSweepSettlement) {
        let Self {
            mut report,
            findings,
            settlement,
        } = self;
        drop(findings);
        let terminal = match report.status {
            IntegrityTier2Status::Complete => IdleSweepTerminal::Complete,
            IntegrityTier2Status::Cancelled => IdleSweepTerminal::Cancelled,
            IntegrityTier2Status::Refused => IdleSweepTerminal::Refused,
        };
        let outcome = settlement.settle(terminal);
        if report.status == IntegrityTier2Status::Complete
            && outcome == IdleSweepSettlement::Superseded
        {
            report = report.cancel();
        }
        (report, outcome)
    }
}

#[must_use = "Tier 2 blocking ownership must be joined through physical completion"]
pub(crate) struct IntegrityTier2BlockingWorker {
    completion: tokio::sync::oneshot::Receiver<IntegrityTier2OwnedResult>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
#[error("Tier 2 dedicated worker stopped before returning sweep ownership")]
pub(crate) struct IntegrityTier2BlockingWorkerUnavailable;

trait IntegrityTier2ThreadSpawner: Send + 'static {
    fn spawn(self, name: &'static str, run: impl FnOnce() + Send + 'static) -> Result<(), ()>;
}

struct SystemIntegrityTier2ThreadSpawner;

impl IntegrityTier2ThreadSpawner for SystemIntegrityTier2ThreadSpawner {
    fn spawn(self, name: &'static str, run: impl FnOnce() + Send + 'static) -> Result<(), ()> {
        std::thread::Builder::new()
            .name(name.to_string())
            .spawn(run)
            .map(drop)
            .map_err(|_| ())
    }
}

impl IntegrityTier2OwnedWork {
    pub(crate) fn new(
        state: AppState,
        ticket: KnownGoodTier2Ticket,
        settlement: IdleSweepSettlementOwner,
    ) -> Result<Self, IntegrityTier2OwnedWorkAuthorityMismatch> {
        if !ticket.matches_settlement(&settlement)
            || !state.known_good_tier2_ticket_is_current(&ticket)
        {
            return Err(IntegrityTier2OwnedWorkAuthorityMismatch { ticket, settlement });
        }
        Ok(Self {
            state,
            ticket,
            settlement,
        })
    }

    pub(crate) fn spawn(self) -> IntegrityTier2BlockingWorker {
        self.spawn_with_platform(SystemLowPriorityPlatform)
    }

    fn spawn_with_platform<Platform>(self, platform: Platform) -> IntegrityTier2BlockingWorker
    where
        Platform: LowPriorityPlatform,
    {
        self.spawn_with_platform_and_spawner(platform, SystemIntegrityTier2ThreadSpawner)
    }

    fn spawn_with_platform_and_spawner<Platform, Spawner>(
        self,
        platform: Platform,
        spawner: Spawner,
    ) -> IntegrityTier2BlockingWorker
    where
        Platform: LowPriorityPlatform,
        Spawner: IntegrityTier2ThreadSpawner,
    {
        let work = Arc::new(Mutex::new(Some(self)));
        let thread_work = work.clone();
        let (completion_tx, completion) = tokio::sync::oneshot::channel();
        let spawned = spawner.spawn("axial-tier-two-integrity", move || {
            let work = thread_work
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .take()
                .expect("Tier 2 worker was already claimed");
            let result = complete_integrity_tier2_owned(work, platform);
            let _ = completion_tx.send(result);
        });

        match spawned {
            Ok(()) => IntegrityTier2BlockingWorker { completion },
            Err(_) => {
                let work = work
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .take()
                    .expect("failed Tier 2 thread spawn must leave work recoverable");
                let result = refuse_integrity_tier2_thread_spawn(work);
                let (ready_tx, ready_completion) = tokio::sync::oneshot::channel();
                let _ = ready_tx.send(result);
                IntegrityTier2BlockingWorker {
                    completion: ready_completion,
                }
            }
        }
    }
}

impl IntegrityTier2BlockingWorker {
    pub(crate) async fn join(
        self,
    ) -> Result<IntegrityTier2OwnedResult, IntegrityTier2BlockingWorkerUnavailable> {
        self.completion
            .await
            .map_err(|_| IntegrityTier2BlockingWorkerUnavailable)
    }
}

fn complete_integrity_tier2_owned<Platform>(
    work: IntegrityTier2OwnedWork,
    platform: Platform,
) -> IntegrityTier2OwnedResult
where
    Platform: LowPriorityPlatform,
{
    let IntegrityTier2OwnedWork {
        state,
        ticket,
        settlement,
    } = work;
    let cancellation = settlement.cancellation();
    let report = run_integrity_tier2_owned(platform, || {
        sense_integrity_tier2_owned(&state, &ticket, &cancellation)
    });
    IntegrityTier2OwnedResult {
        state,
        report,
        ticket,
        settlement,
    }
}

fn refuse_integrity_tier2_thread_spawn(work: IntegrityTier2OwnedWork) -> IntegrityTier2OwnedResult {
    let IntegrityTier2OwnedWork {
        state,
        ticket,
        settlement,
    } = work;
    let report = IntegrityTier2Report::new(0, 0).refuse(tier2_worker_refused_fact());
    IntegrityTier2OwnedResult {
        state,
        report,
        ticket,
        settlement,
    }
}

fn run_integrity_tier2_owned<Platform>(
    platform: Platform,
    run: impl FnOnce() -> IntegrityTier2Report,
) -> IntegrityTier2Report
where
    Platform: LowPriorityPlatform,
{
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        match run_at_low_priority(platform, run) {
            LowPriorityOutcome::Complete(report) => report,
            LowPriorityOutcome::EnterFailed => {
                IntegrityTier2Report::new(0, 0).refuse(tier2_priority_enter_refused_fact())
            }
            LowPriorityOutcome::RestoreFailed(report) => {
                report.refuse(tier2_priority_restore_refused_fact())
            }
        }
    }))
    .unwrap_or_else(|_| IntegrityTier2Report::new(0, 0).refuse(tier2_worker_refused_fact()))
}

impl IntegrityTier2Report {
    fn new(selected_entry_count: usize, expected_content_byte_count: u64) -> Self {
        Self {
            status: IntegrityTier2Status::Refused,
            facts: Vec::new(),
            selected_entry_count,
            verified_entry_count: 0,
            processed_entry_count: 0,
            hashed_entry_count: 0,
            expected_content_byte_count,
            content_read_byte_count: 0,
            metadata_lookup_count: 0,
            link_lookup_count: 0,
            suppressed_fact_count: 0,
            repairable_observations: Vec::new(),
        }
    }

    pub(crate) fn cancel(mut self) -> Self {
        self.status = IntegrityTier2Status::Cancelled;
        self.facts.clear();
        self.suppressed_fact_count = 0;
        self.repairable_observations.clear();
        self
    }

    fn refuse(mut self, fact: ExecutionFact) -> Self {
        self.status = IntegrityTier2Status::Refused;
        self.facts.clear();
        self.suppressed_fact_count = 0;
        self.repairable_observations.clear();
        self.facts.push(fact);
        self
    }

    fn registered_artifact_observations(&self) -> Vec<RegisteredArtifactObservation> {
        self.repairable_observations
            .iter()
            .map(|observed| observed.observation)
            .collect()
    }

    fn bind_registered_artifact_targets(&mut self, findings: &RegisteredArtifactFindings) -> bool {
        let mut bindings = Vec::with_capacity(self.repairable_observations.len());
        for observed in &self.repairable_observations {
            if self.facts.get(observed.fact_index).is_none() {
                return false;
            }
            let Some(target) = findings.target_for(observed.observation) else {
                return false;
            };
            bindings.push((observed.fact_index, target.clone()));
        }
        for (fact_index, target) in bindings {
            self.facts[fact_index].target = Some(target);
        }
        self.repairable_observations.clear();
        true
    }
}

trait IntegrityTier2Pacer {
    fn elapsed(&self) -> Duration;
    fn sleep(&self, duration: Duration);
}

struct SystemIntegrityTier2Pacer {
    started_at: Instant,
}

impl SystemIntegrityTier2Pacer {
    fn start() -> Self {
        Self {
            started_at: Instant::now(),
        }
    }
}

impl IntegrityTier2Pacer for SystemIntegrityTier2Pacer {
    fn elapsed(&self) -> Duration {
        self.started_at.elapsed()
    }

    fn sleep(&self, duration: Duration) {
        std::thread::sleep(duration);
    }
}

struct IntegrityTier2ReadControl<'a, Pacer> {
    cancellation: &'a IdleSweepCancellation,
    pacer: &'a Pacer,
    last_refill: Duration,
    byte_tokens: u128,
    entry_tokens: u128,
}

impl<'a, Pacer: IntegrityTier2Pacer> IntegrityTier2ReadControl<'a, Pacer> {
    fn new(cancellation: &'a IdleSweepCancellation, pacer: &'a Pacer) -> Self {
        Self {
            cancellation,
            pacer,
            last_refill: pacer.elapsed(),
            byte_tokens: u128::from(INTEGRITY_TIER2_BYTE_BURST) * 1_000_000_000,
            entry_tokens: 1_000_000_000,
        }
    }
}

impl<Pacer: IntegrityTier2Pacer> IntegrityTier2ReadControl<'_, Pacer> {
    fn before_entry(&mut self) -> bool {
        self.admit(1, INTEGRITY_TIER2_ENTRIES_PER_SECOND, 1, false)
    }

    fn refill(&mut self) {
        let now = self.pacer.elapsed();
        let elapsed_nanoseconds = now.saturating_sub(self.last_refill).as_nanos();
        self.last_refill = now;
        self.byte_tokens = self
            .byte_tokens
            .saturating_add(
                elapsed_nanoseconds.saturating_mul(u128::from(INTEGRITY_TIER2_BYTES_PER_SECOND)),
            )
            .min(u128::from(INTEGRITY_TIER2_BYTE_BURST) * 1_000_000_000);
        self.entry_tokens = self
            .entry_tokens
            .saturating_add(
                elapsed_nanoseconds.saturating_mul(u128::from(INTEGRITY_TIER2_ENTRIES_PER_SECOND)),
            )
            .min(1_000_000_000);
    }

    fn admit(&mut self, amount: u64, rate_per_second: u64, burst: u64, bytes: bool) -> bool {
        let required = u128::from(amount) * 1_000_000_000;
        debug_assert!(amount <= burst);
        loop {
            if self.cancellation.is_cancelled() {
                return false;
            }
            self.refill();
            let tokens = if bytes {
                &mut self.byte_tokens
            } else {
                &mut self.entry_tokens
            };
            if *tokens >= required {
                *tokens -= required;
                return !self.cancellation.is_cancelled();
            }
            let deficit = required - *tokens;
            let wait_nanoseconds = deficit.div_ceil(u128::from(rate_per_second));
            let wait = Duration::from_nanos(wait_nanoseconds.try_into().unwrap_or(u64::MAX));
            self.pacer
                .sleep(wait.min(MAX_INTEGRITY_TIER2_THROTTLE_SLEEP));
        }
    }
}

impl<Pacer: IntegrityTier2Pacer> ContentReadControl for IntegrityTier2ReadControl<'_, Pacer> {
    fn before_read(&mut self, next_read_bytes: usize) -> bool {
        self.admit(
            next_read_bytes as u64,
            INTEGRITY_TIER2_BYTES_PER_SECOND,
            INTEGRITY_TIER2_BYTE_BURST,
            true,
        )
    }
}

struct IntegrityTier2RunContext<'a, Pacer> {
    library_root: &'a Path,
    runtime_cache: &'a ManagedRuntimeCache,
    inventory: &'a KnownGoodInventory,
    cancellation: &'a IdleSweepCancellation,
    pacer: &'a Pacer,
}

fn sense_integrity_tier2_owned(
    state: &AppState,
    ticket: &KnownGoodTier2Ticket,
    cancellation: &IdleSweepCancellation,
) -> IntegrityTier2Report {
    let (library_root, runtime_cache, inventory) = ticket.execution_parts();
    if cancellation.is_cancelled() {
        return IntegrityTier2Report::new(inventory.entries().len(), 0).cancel();
    }
    let projection = match inventory.tier2_projection() {
        Ok(projection) => projection,
        Err(error) => {
            return IntegrityTier2Report::new(error.entry_count(), 0)
                .refuse(tier2_projection_refused_fact(error.entry_count()));
        }
    };
    let pacer = SystemIntegrityTier2Pacer::start();
    run_integrity_tier2_with(
        projection,
        IntegrityTier2RunContext {
            library_root,
            runtime_cache,
            inventory,
            cancellation,
            pacer: &pacer,
        },
        FilesystemIntegrityReader::default,
        || state.known_good_tier2_ticket_is_current(&ticket),
    )
}

fn run_integrity_tier2_with<Reader, ReaderFactory, Pacer, IsCurrent>(
    projection: Tier2Projection<'_>,
    context: IntegrityTier2RunContext<'_, Pacer>,
    mut reader_factory: ReaderFactory,
    mut is_current: IsCurrent,
) -> IntegrityTier2Report
where
    Reader: MetadataReader + ContentReader,
    ReaderFactory: FnMut() -> Reader,
    Pacer: IntegrityTier2Pacer,
    IsCurrent: FnMut() -> bool,
{
    let mut report = IntegrityTier2Report::new(
        projection.entry_count(),
        projection.expected_content_byte_count(),
    );
    if context.cancellation.is_cancelled() {
        return report.cancel();
    }
    if !is_current() {
        return report.refuse(tier2_authority_refused_fact());
    }

    let mut entries = projection.iter().peekable();
    let mut control = IntegrityTier2ReadControl::new(context.cancellation, context.pacer);
    while entries.peek().is_some() {
        if context.cancellation.is_cancelled() {
            return report.cancel();
        }
        if !is_current() {
            return report.refuse(tier2_authority_refused_fact());
        }
        let reader = reader_factory();
        let batch_start_bytes = report.content_read_byte_count;
        let mut batch_entry_count = 0_usize;
        while batch_entry_count < MAX_INTEGRITY_TIER2_BATCH_ENTRIES && entries.peek().is_some() {
            if context.cancellation.is_cancelled() {
                return report.cancel();
            }
            if !control.before_entry() {
                return report.cancel();
            }
            let projected = entries.next().expect("peeked Tier 2 entry");
            let path = projected.physical_path(context.library_root, context.runtime_cache);
            if !inspect_tier2_entry(
                &reader,
                projected.entry(),
                projected.inventory_ordinal(),
                &path,
                context.inventory,
                &mut control,
                &mut report,
            ) {
                return report.cancel();
            }
            report.processed_entry_count += 1;
            batch_entry_count += 1;
            if report
                .content_read_byte_count
                .saturating_sub(batch_start_bytes)
                >= INTEGRITY_TIER2_BATCH_CONTENT_THRESHOLD_BYTES
            {
                break;
            }
        }
        if context.cancellation.is_cancelled() {
            return report.cancel();
        }
        if MetadataReader::revalidate(&reader).is_err() {
            return report.refuse(tier2_confinement_refused_fact());
        }
        report.verified_entry_count = report.processed_entry_count;
        drop(reader);
        if !is_current() {
            return report.refuse(tier2_authority_refused_fact());
        }
    }
    if context.cancellation.is_cancelled() {
        return report.cancel();
    }
    if !is_current() {
        return report.refuse(tier2_authority_refused_fact());
    }
    report.status = IntegrityTier2Status::Complete;
    report
}

fn inspect_tier2_entry(
    reader: &(impl MetadataReader + ContentReader),
    entry: &KnownGoodEntry,
    inventory_ordinal: usize,
    path: &KnownGoodPhysicalPath,
    inventory: &KnownGoodInventory,
    control: &mut dyn ContentReadControl,
    report: &mut IntegrityTier2Report,
) -> bool {
    match entry.integrity() {
        KnownGoodIntegrity::Sha1 { digest, size }
        | KnownGoodIntegrity::ExactBytes { digest, size } => {
            let byte_budget = report
                .expected_content_byte_count
                .saturating_sub(report.content_read_byte_count);
            let result = reader.hash_file_controlled(path, *size, byte_budget, control);
            report.content_read_byte_count = report
                .content_read_byte_count
                .saturating_add(result.bytes_read);
            let (fact, repair_condition) = match result.observation {
                Ok(ContentHashObservation::Hashed {
                    digest: observed_digest,
                }) => {
                    report.hashed_entry_count += 1;
                    if observed_digest != digest.as_str() {
                        (
                            Some(integrity_fact(
                                entry,
                                inventory_ordinal,
                                ExecutionFactKind::ArtifactHashMismatch,
                                "hash_mismatch",
                            )),
                            Some(RegisteredArtifactCondition::Corrupt),
                        )
                    } else {
                        (None, None)
                    }
                }
                Ok(ContentHashObservation::SizeDrift { observed_size }) => {
                    let mut fact = integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::ArtifactSizeDrift,
                        "size_drift",
                    );
                    fact.fields.extend([
                        public_field("expected_size", size.to_string()),
                        public_field("observed_size", observed_size.to_string()),
                    ]);
                    (Some(fact), Some(RegisteredArtifactCondition::Corrupt))
                }
                Ok(ContentHashObservation::WrongType) => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::ArtifactMissing,
                        "wrong_type",
                    )),
                    Some(RegisteredArtifactCondition::Corrupt),
                ),
                Ok(ContentHashObservation::ChangedDuringRead) => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::PrimitiveRefused,
                        "content_changed_during_read",
                    )),
                    None,
                ),
                Ok(ContentHashObservation::BudgetRefused) => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::PrimitiveRefused,
                        "content_budget_refused",
                    )),
                    None,
                ),
                Ok(ContentHashObservation::Cancelled) => return false,
                Err(error) if error.kind() == io::ErrorKind::NotFound => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::ArtifactMissing,
                        "missing",
                    )),
                    Some(RegisteredArtifactCondition::Missing),
                ),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::FilePermissionDenied,
                        "content_permission_denied",
                    )),
                    None,
                ),
                Err(_) => (
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::PrimitiveRefused,
                        "content_unavailable",
                    )),
                    None,
                ),
            };
            if let Some(fact) = fact {
                let fact_index = push_bounded_tier2_fact(report, fact);
                if let (Some(fact_index), Some(condition)) = (fact_index, repair_condition)
                    && tier2_assets_leaf_is_source_backed(inventory, entry, inventory_ordinal)
                {
                    report
                        .repairable_observations
                        .push(Tier2RepairableObservation {
                            fact_index,
                            observation: RegisteredArtifactObservation::new(
                                inventory_ordinal,
                                condition,
                            ),
                        });
                }
            }
        }
        KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => {
            report.metadata_lookup_count += 1;
            let fact = match reader.symlink_metadata(path) {
                Ok(observation) => {
                    let mut tier0_shape = IntegrityTier0Report::default();
                    let fact = inspect_observation(
                        reader,
                        entry,
                        path,
                        inventory_ordinal,
                        observation,
                        &mut tier0_shape,
                    );
                    report.link_lookup_count += tier0_shape.link_lookup_count;
                    fact
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => Some(integrity_fact(
                    entry,
                    inventory_ordinal,
                    ExecutionFactKind::ArtifactMissing,
                    "missing",
                )),
                Err(error) if error.kind() == io::ErrorKind::PermissionDenied => {
                    Some(integrity_fact(
                        entry,
                        inventory_ordinal,
                        ExecutionFactKind::FilePermissionDenied,
                        "metadata_permission_denied",
                    ))
                }
                Err(_) => Some(integrity_fact(
                    entry,
                    inventory_ordinal,
                    ExecutionFactKind::PrimitiveRefused,
                    "metadata_unavailable",
                )),
            };
            if let Some(fact) = fact {
                let _ = push_bounded_tier2_fact(report, fact);
            }
        }
    }
    true
}

fn tier2_projection_refused_fact(entry_count: usize) -> ExecutionFact {
    tier2_refused_fact(
        "known_good_tier2_projection",
        "tier2_projection_refused",
        Some(entry_count),
    )
}

fn tier2_confinement_refused_fact() -> ExecutionFact {
    tier2_refused_fact(
        "known_good_tier2_path_confinement",
        "path_identity_changed",
        None,
    )
}

fn tier2_authority_refused_fact() -> ExecutionFact {
    tier2_refused_fact("known_good_tier2_authority", "live_authority_changed", None)
}

fn tier2_worker_refused_fact() -> ExecutionFact {
    tier2_refused_fact("known_good_tier2_worker", "tier2_worker_unavailable", None)
}

fn tier2_priority_enter_refused_fact() -> ExecutionFact {
    tier2_refused_fact(
        "known_good_tier2_low_priority",
        "tier2_low_priority_enter_failed",
        None,
    )
}

fn tier2_priority_restore_refused_fact() -> ExecutionFact {
    tier2_refused_fact(
        "known_good_tier2_low_priority",
        "tier2_low_priority_restore_failed",
        None,
    )
}

fn tier2_refused_fact(
    target_id: &'static str,
    observation: &'static str,
    entry_count: Option<usize>,
) -> ExecutionFact {
    let mut fields = vec![public_field("observation", observation)];
    if let Some(entry_count) = entry_count {
        fields.push(public_field(
            "selected_entry_count",
            entry_count.to_string(),
        ));
    }
    ExecutionFact {
        operation_id: None,
        kind: ExecutionFactKind::PrimitiveRefused,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            target_id,
            OwnershipClass::LauncherManaged,
        )),
        fields,
    }
}

fn tier2_assets_leaf_is_source_backed(
    inventory: &KnownGoodInventory,
    entry: &KnownGoodEntry,
    inventory_ordinal: usize,
) -> bool {
    matches!(
        (entry.root(), entry.kind()),
        (
            KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetIndex | KnownGoodArtifactKind::AssetObject,
        )
    ) && inventory
        .bind_standalone_leaf_repair_source(inventory_ordinal)
        .is_ok_and(|source| source.root() == entry.root() && source.kind() == entry.kind())
}

fn push_bounded_tier2_fact(
    report: &mut IntegrityTier2Report,
    fact: ExecutionFact,
) -> Option<usize> {
    if report.facts.len() < MAX_INTEGRITY_TIER2_FACTS {
        let fact_index = report.facts.len();
        report.facts.push(fact);
        Some(fact_index)
    } else {
        report.suppressed_fact_count += 1;
        None
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
    use crate::guardian::{
        ActionPlanPrerequisite, DiagnosisId, GuardianAction, GuardianActionKind,
        GuardianActionPlan, GuardianArtifactRepairSettlement, GuardianArtifactRepairStatus,
        GuardianConfidence, GuardianDecision, GuardianMode,
        execute_failed_managed_assets_component_rebuild_for_test,
        execute_registered_guardian_artifact_repair,
    };
    use crate::state::contracts::{
        OperationId, ReconciliationComponent, ReconciliationRung, ReconciliationScope,
        ReconciliationTerminalOutcome, StabilizationSystem,
    };
    use crate::state::{
        AppState, AppStateInit, IdleSweepReservation, IdleSweepSettlement,
        IdleSweepSettlementOwner, IdleSweepTerminal, InstallStore, RegisteredArtifactFindings,
        RegisteredArtifactRepairAdmission, RegisteredArtifactRepairAuthorizationRejection,
        RegisteredArtifactRepairEffect, SessionStore,
    };
    use axial_config::{AppConfig, AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_minecraft::known_good::{
        KnownGoodInventory, TestKnownGoodEntry, TestKnownGoodIntegrity, TestKnownGoodRoot,
    };
    use axial_performance::PerformanceManager;
    use std::collections::HashMap;
    use std::fs;
    use std::io::Write as _;
    use std::net::{TcpListener, TcpStream};
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};
    use std::sync::{Arc, Condvar, Mutex, mpsc};
    use std::thread;
    use std::time::{Duration, Instant};

    #[test]
    fn tier0_worker_ranges_are_balanced_bounded_and_exact() {
        for item_count in [0, 1, 3, 4, 5, 511, 512] {
            let ranges = tier0_worker_ranges(item_count);
            assert!(ranges.len() <= WINDOWS_TIER0_WORKERS);
            let covered = ranges
                .iter()
                .flat_map(|range| range.clone())
                .collect::<Vec<_>>();
            assert_eq!(covered, (0..item_count).collect::<Vec<_>>());
            let lengths = ranges.iter().map(Range::len).collect::<Vec<_>>();
            if let (Some(minimum), Some(maximum)) = (lengths.iter().min(), lengths.iter().max()) {
                assert!(maximum - minimum <= 1);
            }
        }
    }

    #[test]
    fn indexed_tier0_results_restore_order_and_refuse_bad_cardinality() {
        assert_eq!(
            order_indexed_results(5, [(4, 'e'), (2, 'c'), (0, 'a'), (3, 'd'), (1, 'b')]),
            Ok(vec!['a', 'b', 'c', 'd', 'e'])
        );
        assert_eq!(order_indexed_results(2, [(0, 'a')]), Err(()));
        assert_eq!(order_indexed_results(2, [(0, 'a'), (0, 'b')]), Err(()));
        assert_eq!(order_indexed_results(2, [(0, 'a'), (2, 'c')]), Err(()));
    }

    fn tier2_cancellation() -> IdleSweepCancellation {
        IdleSweepCancellation::new_for_test()
    }

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

    #[derive(Clone)]
    struct ScriptedTier2Reader {
        metadata: Arc<ScriptedReader>,
        content: Arc<ScriptedContentReader>,
    }

    impl MetadataReader for ScriptedTier2Reader {
        fn symlink_metadata(
            &self,
            path: &KnownGoodPhysicalPath,
        ) -> io::Result<MetadataObservation> {
            self.metadata.symlink_metadata(path)
        }

        fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf> {
            self.metadata.read_link(path)
        }

        fn revalidate(&self) -> io::Result<()> {
            self.metadata.revalidate()?;
            self.content.revalidate()
        }
    }

    impl ContentReader for ScriptedTier2Reader {
        fn hash_file(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
        ) -> ContentHashResult {
            self.content.hash_file(path, expected_size, byte_budget)
        }

        fn hash_file_controlled(
            &self,
            path: &KnownGoodPhysicalPath,
            expected_size: u64,
            byte_budget: u64,
            control: &mut dyn ContentReadControl,
        ) -> ContentHashResult {
            let result = self.content.hash_file(path, expected_size, byte_budget);
            let mut admitted = 0_u64;
            while admitted < result.bytes_read {
                let next = (result.bytes_read - admitted).min(64 * 1024) as usize;
                if !control.before_read(next) {
                    return ContentHashResult {
                        observation: Ok(ContentHashObservation::Cancelled),
                        bytes_read: admitted,
                    };
                }
                admitted += next as u64;
            }
            result
        }

        fn revalidate(&self) -> io::Result<()> {
            self.metadata.revalidate()?;
            self.content.revalidate()
        }
    }

    struct ScriptedTier2Pacer {
        elapsed: Mutex<Duration>,
        sleeps: Mutex<Vec<Duration>>,
        cancellation: Option<IdleSweepCancellation>,
        cancel_on_sleep: usize,
    }

    impl ScriptedTier2Pacer {
        fn new() -> Self {
            Self {
                elapsed: Mutex::new(Duration::ZERO),
                sleeps: Mutex::new(Vec::new()),
                cancellation: None,
                cancel_on_sleep: usize::MAX,
            }
        }

        fn cancelling_on(cancellation: IdleSweepCancellation, cancel_on_sleep: usize) -> Self {
            Self {
                cancellation: Some(cancellation),
                cancel_on_sleep,
                ..Self::new()
            }
        }

        fn advance(&self, duration: Duration) {
            *self.elapsed.lock().expect("scripted elapsed") += duration;
        }
    }

    impl IntegrityTier2Pacer for ScriptedTier2Pacer {
        fn elapsed(&self) -> Duration {
            *self.elapsed.lock().expect("scripted elapsed")
        }

        fn sleep(&self, duration: Duration) {
            let mut sleeps = self.sleeps.lock().expect("scripted sleeps");
            sleeps.push(duration);
            if sleeps.len() == self.cancel_on_sleep
                && let Some(cancellation) = &self.cancellation
            {
                cancellation.cancel();
            }
            drop(sleeps);
            *self.elapsed.lock().expect("scripted elapsed") += duration;
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

    #[derive(Clone)]
    struct ScriptedLowPriorityPlatform {
        enter_fails: bool,
        restore_failures: Arc<Mutex<usize>>,
        gate: Option<Arc<BlockingContentGate>>,
        events: Arc<Mutex<Vec<&'static str>>>,
    }

    impl ScriptedLowPriorityPlatform {
        fn successful() -> Self {
            Self {
                enter_fails: false,
                restore_failures: Arc::new(Mutex::new(0)),
                gate: None,
                events: Arc::new(Mutex::new(Vec::new())),
            }
        }

        fn enter_failure() -> Self {
            Self {
                enter_fails: true,
                ..Self::successful()
            }
        }

        fn restore_failure() -> Self {
            Self {
                restore_failures: Arc::new(Mutex::new(1)),
                ..Self::successful()
            }
        }

        fn gated(gate: Arc<BlockingContentGate>) -> Self {
            Self {
                gate: Some(gate),
                ..Self::successful()
            }
        }

        fn events(&self) -> Vec<&'static str> {
            self.events.lock().expect("priority events").clone()
        }
    }

    impl LowPriorityPlatform for ScriptedLowPriorityPlatform {
        type Saved = ();

        fn enter(&self) -> Result<Self::Saved, ()> {
            self.events.lock().expect("priority events").push("enter");
            if self.enter_fails {
                return Err(());
            }
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            Ok(())
        }

        fn restore(&self, _saved: &Self::Saved) -> Result<(), ()> {
            self.events.lock().expect("priority events").push("restore");
            let mut failures = self.restore_failures.lock().expect("restore failures");
            if *failures == 0 {
                Ok(())
            } else {
                *failures -= 1;
                Err(())
            }
        }
    }

    #[derive(Clone, Default)]
    struct RefusingTier2ThreadSpawner {
        names: Arc<Mutex<Vec<&'static str>>>,
    }

    impl IntegrityTier2ThreadSpawner for RefusingTier2ThreadSpawner {
        fn spawn(self, name: &'static str, _run: impl FnOnce() + Send + 'static) -> Result<(), ()> {
            self.names.lock().expect("thread names").push(name);
            Err(())
        }
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
        inner: FilesystemIntegrityReader,
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
            MetadataReader::revalidate(&self.inner)
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

    async fn tier2_owned_work_fixture(label: &str) -> (AppState, PathBuf, IntegrityTier2OwnedWork) {
        let (state, root) = state_fixture(label, None);
        let managed_parent = root.join("private-library-root/libraries/owned");
        fs::create_dir_all(&managed_parent).expect("managed library parent");
        fs::write(managed_parent.join("library.jar"), [7_u8]).expect("managed library");
        let instance = state
            .instances()
            .insert_for_test("Tier two owned work", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "owned/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 1 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let idle_epoch = state.subscribe_integrity_idle().borrow().epoch();
        let producer = state
            .try_claim_producer()
            .expect("claim idle sweep producer");
        let reservation = state
            .try_reserve_idle_sweep(idle_epoch, producer)
            .expect("idle sweep reservation");
        let settlement = IdleSweepSettlementOwner::new(reservation);
        let ticket = state
            .mint_known_good_tier2_ticket(&settlement.authority(), &instance.id)
            .await
            .expect("Tier 2 ticket");
        let work = IntegrityTier2OwnedWork::new(state.clone(), ticket, settlement)
            .expect("matching Tier 2 ownership");
        (state, root, work)
    }

    async fn tier2_assets_owned_work_fixture(
        label: &str,
    ) -> (AppState, PathBuf, IntegrityTier2OwnedWork) {
        let (state, root) = state_fixture(label, None);
        let instance = state
            .instances()
            .insert_for_test("Tier two Assets owned work", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Assets,
            "indexes/owned.json",
            KnownGoodArtifactKind::AssetIndex,
            TestKnownGoodIntegrity::Sha1 {
                digest: ZERO_SHA1.to_string(),
                size: 1,
            },
        )])
        .expect("Assets inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/indexes/owned.json")
        .expect("Assets repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let idle_epoch = state.subscribe_integrity_idle().borrow().epoch();
        let producer = state
            .try_claim_producer()
            .expect("claim idle sweep producer");
        let reservation = state
            .try_reserve_idle_sweep(idle_epoch, producer)
            .expect("idle sweep reservation");
        let settlement = IdleSweepSettlementOwner::new(reservation);
        let ticket = state
            .mint_known_good_tier2_ticket(&settlement.authority(), &instance.id)
            .await
            .expect("Tier 2 ticket");
        let work = IntegrityTier2OwnedWork::new(state.clone(), ticket, settlement)
            .expect("matching Tier 2 ownership");
        (state, root, work)
    }

    async fn settle_tier2_result(
        result: IntegrityTier2OwnedResult,
    ) -> (IntegrityTier2Report, IdleSweepSettlement) {
        result.settle_without_recovery().await
    }

    async fn test_integrity_foreground(state: &AppState) -> IntegrityForegroundLease {
        state
            .register_integrity_foreground()
            .expect("register test integrity foreground")
            .wait_for_settlement()
            .await
    }

    async fn mint_test_verification_lease(
        state: &AppState,
        lifecycle: &InstanceLifecycleLease,
        expected_library_root: &Path,
    ) -> KnownGoodVerificationLease {
        let foreground = test_integrity_foreground(state).await;
        state
            .mint_known_good_verification_lease(&foreground, lifecycle, expected_library_root)
            .expect("mint test verification lease")
    }

    async fn seal_registered_leaf_finding(
        state: &AppState,
        lifecycle: &InstanceLifecycleLease,
        expected_library_root: &Path,
        inventory_ordinal: usize,
        condition: RegisteredArtifactCondition,
    ) -> RegisteredArtifactFindings {
        let lease = mint_test_verification_lease(state, lifecycle, expected_library_root).await;
        let observation = lease
            .registered_artifact_observation(inventory_ordinal, condition)
            .expect("source-backed registered leaf observation");
        state
            .seal_registered_artifact_findings(lease, vec![observation])
            .expect("sealed registered leaf finding")
    }

    async fn corrupt_assets_sweep_leaf_fixture(
        label: &str,
    ) -> (
        AppState,
        PathBuf,
        IdleSweepReservation,
        RegisteredArtifactRepairAdmission,
    ) {
        let (state, root) = state_fixture(label, None);
        let instance = state
            .instances()
            .insert_for_test("Sweep corrupt Assets leaf", "1.21.5")
            .expect("instance");
        let expected = b"expected-sweep-asset";
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Assets,
            "indexes/sweep-corrupt.json",
            KnownGoodArtifactKind::AssetIndex,
            TestKnownGoodIntegrity::Sha1 {
                digest: format!("{:x}", Sha1::digest(expected)),
                size: expected.len() as u64,
            },
        )])
        .expect("sweep Assets inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/sweep-corrupt.json")
        .expect("sweep Assets source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let destination = root.join("private-library-root/assets/indexes/sweep-corrupt.json");
        fs::create_dir_all(destination.parent().expect("sweep Assets parent"))
            .expect("sweep Assets parent");
        fs::write(destination, vec![b'x'; expected.len()]).expect("corrupt sweep Assets index");
        let epoch = state.subscribe_integrity_idle().borrow().epoch();
        let reservation = state
            .try_reserve_idle_sweep(
                epoch,
                state.try_claim_producer().expect("claim sweep producer"),
            )
            .expect("sweep reservation");
        let ticket = state
            .mint_known_good_tier2_ticket(&reservation.authority(), &instance.id)
            .await
            .expect("Tier 2 ticket");
        let findings = state
            .seal_tier2_registered_artifact_request(Tier2RegisteredArtifactSealRequest::new(
                ticket,
                vec![RegisteredArtifactObservation::new(
                    0,
                    RegisteredArtifactCondition::Corrupt,
                )],
            ))
            .await
            .expect("sweep findings");
        let target = findings.repair_target().expect("repair target").clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(target, GuardianMode::Managed))
            .expect("repair authorization");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                OperationId::new(format!("{label}-leaf")),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("leaf admission");
        (state, root, reservation, admission)
    }

    const ZERO_SHA1: &str = "0000000000000000000000000000000000000000";
    const NONZERO_SHA1: &str = "1111111111111111111111111111111111111111";

    struct RegisteredArtifactServer {
        url: String,
        stop: mpsc::Sender<()>,
        worker: thread::JoinHandle<()>,
    }

    impl RegisteredArtifactServer {
        fn start(body: Vec<u8>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind artifact server");
            listener
                .set_nonblocking(true)
                .expect("nonblocking artifact server");
            let url = format!(
                "http://{}/artifact.jar",
                listener.local_addr().expect("artifact server address")
            );
            let (stop, stopped) = mpsc::channel();
            let worker = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => respond_registered_artifact(stream, &body),
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                            if stopped.try_recv().is_ok() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(5));
                        }
                        Err(error) => panic!("artifact server accept: {error}"),
                    }
                }
            });
            Self { url, stop, worker }
        }

        fn close(self) {
            self.stop.send(()).expect("stop artifact server");
            self.worker.join().expect("join artifact server");
        }
    }

    fn respond_registered_artifact(mut stream: TcpStream, body: &[u8]) {
        let mut request = [0_u8; 1024];
        let _ = stream.read(&mut request);
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        )
        .expect("artifact response header");
        stream.write_all(body).expect("artifact response body");
    }

    fn registered_artifact_decision(
        target: TargetDescriptor,
        mode: GuardianMode,
    ) -> GuardianDecision {
        GuardianDecision::for_test(
            None,
            mode,
            GuardianActionKind::Repair,
            vec![DiagnosisId::LauncherManagedArtifactCorrupt],
            Some(GuardianActionPlan::new(
                StabilizationSystem::Guardian,
                ActionPlanPrerequisite {
                    diagnosis_id: DiagnosisId::LauncherManagedArtifactCorrupt,
                    ownership: OwnershipClass::LauncherManaged,
                    confidence: GuardianConfidence::Confirmed,
                    affected_targets: vec![target.clone()],
                    candidate_actions: vec![GuardianActionKind::Repair],
                },
                vec![GuardianAction {
                    kind: GuardianActionKind::Repair,
                    target: Some(target),
                    reason: DiagnosisId::LauncherManagedArtifactCorrupt,
                }],
            )),
        )
    }

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

    #[test]
    fn tier_two_verifies_every_inventory_integrity_shape_in_one_bounded_stream() {
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.json",
                KnownGoodArtifactKind::VersionMetadata,
                TestKnownGoodIntegrity::ExactBytes { size: 1 },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "objects/00/object",
                KnownGoodArtifactKind::AssetObject,
                TestKnownGoodIntegrity::File { size: 2 },
            ),
            entry(
                TestKnownGoodRoot::ManagedRuntime {
                    component: "java-runtime-delta".to_string(),
                },
                "bin/java-real",
                KnownGoodArtifactKind::RuntimeExecutable,
                TestKnownGoodIntegrity::File { size: 3 },
            ),
            entry(
                TestKnownGoodRoot::ManagedRuntime {
                    component: "java-runtime-delta".to_string(),
                },
                "lib",
                KnownGoodArtifactKind::RuntimeDirectory,
                TestKnownGoodIntegrity::Directory,
            ),
            entry(
                TestKnownGoodRoot::ManagedRuntime {
                    component: "java-runtime-delta".to_string(),
                },
                "bin/java",
                KnownGoodArtifactKind::RuntimeLink,
                TestKnownGoodIntegrity::LinkTarget("java-real".to_string()),
            ),
        ])
        .expect("Tier 2 inventory");
        let projection = inventory.tier2_projection().expect("Tier 2 projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new(
                [
                    ("lib", observation(MetadataKind::Directory, 0)),
                    ("bin/java", observation(MetadataKind::Link, 0)),
                ],
                [("bin/java", Ok("java-real"))],
            )),
            content: Arc::new(ScriptedContentReader::new([
                ("1.21.5/1.21.5.json", ScriptedContent::Hashed(ZERO_SHA1, 1)),
                ("objects/00/object", ScriptedContent::Hashed(ZERO_SHA1, 2)),
                ("bin/java-real", ScriptedContent::Hashed(ZERO_SHA1, 3)),
            ])),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.selected_entry_count, 5);
        assert_eq!(report.processed_entry_count, 5);
        assert_eq!(report.verified_entry_count, 5);
        assert_eq!(report.hashed_entry_count, 3);
        assert_eq!(report.expected_content_byte_count, 6);
        assert_eq!(report.content_read_byte_count, 6);
        assert_eq!(report.metadata_lookup_count, 2);
        assert_eq!(report.link_lookup_count, 1);
        assert_eq!(report.suppressed_fact_count, 0);
        assert!(report.facts.is_empty());
        assert_eq!(
            reader
                .content
                .content_paths
                .lock()
                .expect("content paths")
                .len(),
            3
        );
    }

    #[test]
    fn tier_two_retains_only_source_backed_assets_leaf_observations() {
        let empty_sha1 = "da39a3ee5e6b4b0d3255bfef95601890afd80709";
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Assets,
                "indexes/missing.json",
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "indexes/size-drift.json",
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "objects/00/0000000000000000000000000000000000000000",
                KnownGoodArtifactKind::AssetObject,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "objects/11/1111111111111111111111111111111111111111",
                KnownGoodArtifactKind::AssetObject,
                TestKnownGoodIntegrity::Sha1 {
                    digest: NONZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709",
                KnownGoodArtifactKind::AssetObject,
                TestKnownGoodIntegrity::Sha1 {
                    digest: empty_sha1.to_string(),
                    size: 0,
                },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "org/example/library.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
            entry(
                TestKnownGoodRoot::Assets,
                "indexes/source-free.json",
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            ),
        ])
        .expect("Tier 2 repairable inventory");
        let ordinal = |root: &KnownGoodRoot, kind, path: &str| {
            inventory
                .entries()
                .iter()
                .position(|entry| {
                    entry.root() == root && entry.kind() == kind && entry.path().as_str() == path
                })
                .expect("canonical repairable inventory ordinal")
        };
        let missing_index_ordinal = ordinal(
            &KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetIndex,
            "indexes/missing.json",
        );
        let drifted_index_ordinal = ordinal(
            &KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetIndex,
            "indexes/size-drift.json",
        );
        let mismatched_object_ordinal = ordinal(
            &KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetObject,
            "objects/00/0000000000000000000000000000000000000000",
        );
        let wrong_type_object_ordinal = ordinal(
            &KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetObject,
            "objects/11/1111111111111111111111111111111111111111",
        );
        let zero_object_ordinal = ordinal(
            &KnownGoodRoot::Assets,
            KnownGoodArtifactKind::AssetObject,
            "objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709",
        );
        let library_ordinal = ordinal(
            &KnownGoodRoot::Libraries,
            KnownGoodArtifactKind::Library,
            "org/example/library.jar",
        );
        let inventory = inventory
        .with_test_standalone_leaf_repair_source(
            missing_index_ordinal,
            "https://example.invalid/indexes/missing.json",
        )
        .expect("missing index source")
        .with_test_standalone_leaf_repair_source(
            drifted_index_ordinal,
            "https://example.invalid/indexes/size-drift.json",
        )
        .expect("drifted index source")
        .with_test_standalone_leaf_repair_source(
            mismatched_object_ordinal,
            "https://resources.download.minecraft.net/00/0000000000000000000000000000000000000000",
        )
        .expect("mismatched object source")
        .with_test_standalone_leaf_repair_source(
            wrong_type_object_ordinal,
            "https://resources.download.minecraft.net/11/1111111111111111111111111111111111111111",
        )
        .expect("wrong-type object source")
        .with_test_standalone_leaf_repair_source(
            zero_object_ordinal,
            "https://resources.download.minecraft.net/da/da39a3ee5e6b4b0d3255bfef95601890afd80709",
        )
        .expect("zero-byte object source")
        .with_test_standalone_leaf_repair_source(
            library_ordinal,
            "https://example.invalid/org/example/library.jar",
        )
        .expect("library source");
        let projection = inventory.tier2_projection().expect("Tier 2 projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(ScriptedContentReader::new([
                (
                    "indexes/missing.json",
                    ScriptedContent::Error(io::ErrorKind::NotFound),
                ),
                (
                    "indexes/size-drift.json",
                    ScriptedContent::SizeDriftAfterRead {
                        observed_size: 2,
                        bytes_read: 1,
                    },
                ),
                (
                    "objects/00/0000000000000000000000000000000000000000",
                    ScriptedContent::Hashed(NONZERO_SHA1, 1),
                ),
                (
                    "objects/11/1111111111111111111111111111111111111111",
                    ScriptedContent::WrongType,
                ),
                (
                    "objects/da/da39a3ee5e6b4b0d3255bfef95601890afd80709",
                    ScriptedContent::Error(io::ErrorKind::NotFound),
                ),
                (
                    "org/example/library.jar",
                    ScriptedContent::Error(io::ErrorKind::NotFound),
                ),
                (
                    "indexes/source-free.json",
                    ScriptedContent::Error(io::ErrorKind::NotFound),
                ),
            ])),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.facts.len(), 7);
        let mut expected = vec![
            RegisteredArtifactObservation::new(
                missing_index_ordinal,
                RegisteredArtifactCondition::Missing,
            ),
            RegisteredArtifactObservation::new(
                drifted_index_ordinal,
                RegisteredArtifactCondition::Corrupt,
            ),
            RegisteredArtifactObservation::new(
                mismatched_object_ordinal,
                RegisteredArtifactCondition::Corrupt,
            ),
            RegisteredArtifactObservation::new(
                wrong_type_object_ordinal,
                RegisteredArtifactCondition::Corrupt,
            ),
            RegisteredArtifactObservation::new(
                zero_object_ordinal,
                RegisteredArtifactCondition::Missing,
            ),
        ];
        expected.sort_by_key(|observation| observation.inventory_ordinal());
        assert_eq!(report.registered_artifact_observations(), expected);
    }

    #[test]
    fn tier_two_pre_cancel_opens_nothing_and_publishes_no_partial_facts() {
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "org/example/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 1 },
        )])
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let cancellation = tier2_cancellation();
        cancellation.cancel();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let reader_count = AtomicUsize::new(0);

        let report = run_integrity_tier2_with::<ScriptedTier2Reader, _, _, _>(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || {
                reader_count.fetch_add(1, AtomicOrdering::SeqCst);
                panic!("pre-cancelled sweep must not create a reader")
            },
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Cancelled);
        assert_eq!(report.processed_entry_count, 0);
        assert_eq!(report.content_read_byte_count, 0);
        assert!(report.facts.is_empty());
        assert_eq!(reader_count.load(AtomicOrdering::SeqCst), 0);
    }

    #[test]
    fn tier_two_cancels_inside_throttled_content_reads_with_ten_millisecond_waits() {
        let content_size = 2 * 64 * 1024;
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "org/example/large.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: content_size },
        )])
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(ScriptedContentReader::new([(
                "org/example/large.jar",
                ScriptedContent::Hashed(ZERO_SHA1, content_size),
            )])),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::cancelling_on(cancellation.clone(), 1);
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Cancelled);
        assert_eq!(report.processed_entry_count, 0);
        assert_eq!(report.content_read_byte_count, 64 * 1024);
        assert!(report.facts.is_empty());
        let sleeps = pacer.sleeps.lock().expect("scripted sleeps");
        assert_eq!(sleeps.len(), 1);
        assert!(
            sleeps
                .iter()
                .all(|duration| *duration <= MAX_INTEGRITY_TIER2_THROTTLE_SLEEP)
        );
    }

    #[test]
    fn tier_two_rate_limiter_caps_the_complete_stream_at_eight_mebibytes_per_second() {
        let content_size = 2 * 64 * 1024;
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "org/example/rate.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: content_size },
        )])
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(ScriptedContentReader::new([(
                "org/example/rate.jar",
                ScriptedContent::Hashed(ZERO_SHA1, content_size),
            )])),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.content_read_byte_count, content_size);
        let minimum_elapsed = Duration::from_secs_f64(
            (content_size - INTEGRITY_TIER2_BYTE_BURST) as f64
                / INTEGRITY_TIER2_BYTES_PER_SECOND as f64,
        );
        assert!(pacer.elapsed() >= minimum_elapsed);
        assert!(
            pacer
                .sleeps
                .lock()
                .expect("scripted sleeps")
                .iter()
                .all(|duration| *duration <= MAX_INTEGRITY_TIER2_THROTTLE_SLEEP)
        );
    }

    #[test]
    fn tier_two_limiter_never_accrues_more_than_one_content_chunk_of_credit() {
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let mut control = IntegrityTier2ReadControl::new(&cancellation, &pacer);

        assert!(control.before_read(64 * 1024));
        pacer.advance(Duration::from_secs(30));
        assert!(control.before_read(64 * 1024));
        let before = pacer.elapsed();
        assert!(control.before_read(64 * 1024));

        assert!(pacer.elapsed() > before);
        assert!(
            pacer
                .sleeps
                .lock()
                .expect("scripted sleeps")
                .iter()
                .all(|duration| *duration <= MAX_INTEGRITY_TIER2_THROTTLE_SLEEP)
        );
    }

    #[test]
    fn tier_two_limiter_caps_zero_byte_entry_iops_at_sixty_four_per_second() {
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let mut control = IntegrityTier2ReadControl::new(&cancellation, &pacer);

        assert!(control.before_entry());
        let before = pacer.elapsed();
        assert!(control.before_entry());

        assert!(
            pacer.elapsed().saturating_sub(before)
                >= Duration::from_secs_f64(1.0 / INTEGRITY_TIER2_ENTRIES_PER_SECOND as f64)
        );
        assert!(
            pacer
                .sleeps
                .lock()
                .expect("scripted sleeps")
                .iter()
                .all(|duration| *duration <= MAX_INTEGRITY_TIER2_THROTTLE_SLEEP)
        );
    }

    #[test]
    fn tier_two_processes_all_entries_while_bounding_facts_and_reader_batches() {
        let inventory = KnownGoodInventory::from_test_entries((0..=128).map(|index| {
            entry(
                TestKnownGoodRoot::Libraries,
                &format!("bounded/{index:03}.jar"),
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 1 },
            )
        }))
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(
                ScriptedContentReader::new([])
                    .with_default(ScriptedContent::Hashed(NONZERO_SHA1, 1)),
            ),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let reader_count = AtomicUsize::new(0);

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || {
                reader_count.fetch_add(1, AtomicOrdering::SeqCst);
                reader.clone()
            },
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.processed_entry_count, 129);
        assert_eq!(report.verified_entry_count, 129);
        assert_eq!(report.hashed_entry_count, 129);
        assert_eq!(report.facts.len(), MAX_INTEGRITY_TIER2_FACTS);
        assert_eq!(report.suppressed_fact_count, 65);
        assert_eq!(reader_count.load(AtomicOrdering::SeqCst), 2);
        assert!(
            report
                .facts
                .iter()
                .all(|fact| fact.kind == ExecutionFactKind::ArtifactHashMismatch)
        );
    }

    #[test]
    fn tier_two_suppression_and_noncomplete_reports_discard_repair_sidecars() {
        let mut inventory = KnownGoodInventory::from_test_entries((0..=64).map(|index| {
            entry(
                TestKnownGoodRoot::Assets,
                &format!("indexes/bounded-{index:03}.json"),
                KnownGoodArtifactKind::AssetIndex,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 1,
                },
            )
        }))
        .expect("bounded Assets inventory");
        for index in 0..=64 {
            let path = format!("indexes/bounded-{index:03}.json");
            let inventory_ordinal = inventory
                .entries()
                .iter()
                .position(|entry| {
                    entry.root() == &KnownGoodRoot::Assets
                        && entry.kind() == KnownGoodArtifactKind::AssetIndex
                        && entry.path().as_str() == path
                })
                .expect("canonical bounded Assets ordinal");
            inventory = inventory
                .with_test_standalone_leaf_repair_source(
                    inventory_ordinal,
                    &format!("https://example.invalid/{path}"),
                )
                .expect("bounded Assets source");
        }
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(
                ScriptedContentReader::new([])
                    .with_default(ScriptedContent::Hashed(NONZERO_SHA1, 1)),
            ),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.facts.len(), MAX_INTEGRITY_TIER2_FACTS);
        assert_eq!(report.suppressed_fact_count, 1);
        assert_eq!(
            report.registered_artifact_observations().len(),
            MAX_INTEGRITY_TIER2_FACTS
        );
        assert!(
            report
                .registered_artifact_observations()
                .iter()
                .all(|observation| observation.inventory_ordinal() < MAX_INTEGRITY_TIER2_FACTS)
        );

        let cancelled = report.cancel();
        assert!(cancelled.facts.is_empty());
        assert!(cancelled.registered_artifact_observations().is_empty());

        let mut refused = IntegrityTier2Report::new(1, 1);
        refused
            .repairable_observations
            .push(Tier2RepairableObservation {
                fact_index: 0,
                observation: RegisteredArtifactObservation::new(
                    0,
                    RegisteredArtifactCondition::Missing,
                ),
            });
        let refused = refused.refuse(tier2_worker_refused_fact());
        assert!(refused.registered_artifact_observations().is_empty());
    }

    #[test]
    fn tier_two_stale_authority_discards_previously_sensed_artifact_facts() {
        let inventory = KnownGoodInventory::from_test_entries((0..=128).map(|index| {
            entry(
                TestKnownGoodRoot::Libraries,
                &format!("stale/{index:03}.jar"),
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 0 },
            )
        }))
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(ScriptedReader::new([], [])),
            content: Arc::new(
                ScriptedContentReader::new([])
                    .with_default(ScriptedContent::Hashed(NONZERO_SHA1, 0)),
            ),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let current_checks = AtomicUsize::new(0);

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || current_checks.fetch_add(1, AtomicOrdering::SeqCst) < 2,
        );

        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.processed_entry_count, 128);
        assert_eq!(report.verified_entry_count, 128);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("live_authority_changed")
        );
        assert_eq!(report.suppressed_fact_count, 0);
    }

    #[test]
    fn tier_two_batch_revalidation_refusal_discards_artifact_observations() {
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "drift/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 1 },
        )])
        .expect("inventory");
        let projection = inventory.tier2_projection().expect("projection");
        let reader = ScriptedTier2Reader {
            metadata: Arc::new(
                ScriptedReader::new([], []).with_revalidate_error(io::ErrorKind::PermissionDenied),
            ),
            content: Arc::new(
                ScriptedContentReader::new([])
                    .with_default(ScriptedContent::Hashed(NONZERO_SHA1, 1)),
            ),
        };
        let cancellation = tier2_cancellation();
        let pacer = ScriptedTier2Pacer::new();
        let runtime_cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");

        let report = run_integrity_tier2_with(
            projection,
            IntegrityTier2RunContext {
                library_root: Path::new("/private/library"),
                runtime_cache: &runtime_cache,
                inventory: &inventory,
                cancellation: &cancellation,
                pacer: &pacer,
            },
            || reader.clone(),
            || true,
        );

        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.processed_entry_count, 1);
        assert_eq!(report.verified_entry_count, 0);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("path_identity_changed")
        );
    }

    #[tokio::test]
    async fn tier_two_worker_returns_current_unsettled_ownership_without_lifecycle() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-owned-work").await;

        let result = work.spawn().join().await.expect("dedicated worker result");
        assert!(result.settlement.is_current());
        let instance_id = state
            .instances()
            .list()
            .into_iter()
            .next()
            .expect("fixture instance")
            .id;
        let lifecycle = tokio::time::timeout(
            Duration::from_millis(100),
            state.acquire_instance_lifecycle(&instance_id),
        )
        .await
        .expect("worker and completion ticket retain no instance lifecycle");
        drop(lifecycle);
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Authoritative);
        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(report.selected_entry_count, 1);
        assert_eq!(report.verified_entry_count, 1);
        assert_eq!(report.hashed_entry_count, 1);
        assert_eq!(report.content_read_byte_count, 1);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(
            report.facts[0].kind,
            ExecutionFactKind::ArtifactHashMismatch
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn dropping_tier_two_owned_result_abandons_and_cancels_sweep_settlement() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-dropped-result").await;

        let result = work.spawn().join().await.expect("dedicated worker result");
        assert!(result.settlement.is_current());
        assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
        let cancellation = result.settlement.cancellation();

        drop(result);

        assert!(cancellation.is_cancelled());
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_worker_rejects_a_ticket_spliced_to_another_settlement() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-spliced-authority").await;
        let IntegrityTier2OwnedWork {
            state: work_state,
            ticket,
            settlement,
        } = work;
        settlement.settle(IdleSweepTerminal::Cancelled);
        let epoch = state.subscribe_integrity_idle().borrow().epoch();
        let replacement = state
            .try_reserve_idle_sweep(
                epoch,
                state
                    .try_claim_producer()
                    .expect("claim replacement producer"),
            )
            .expect("replacement reservation");
        let replacement = IdleSweepSettlementOwner::new(replacement);
        let cancellation = replacement.cancellation();

        let mismatch = match IntegrityTier2OwnedWork::new(work_state, ticket, replacement) {
            Ok(_) => panic!("ticket and settlement capabilities must not splice"),
            Err(mismatch) => mismatch,
        };
        assert_eq!(mismatch.settle(), IntegrityTier2OwnedWorkRejection::Refused);
        assert!(!cancellation.is_cancelled());
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_worker_classifies_cancellation_between_ticket_and_work_admission() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-cancel-before-admission").await;
        let IntegrityTier2OwnedWork {
            state: work_state,
            ticket,
            settlement,
        } = work;
        let cancellation = settlement.cancellation();
        cancellation.cancel();

        let mismatch = match IntegrityTier2OwnedWork::new(work_state, ticket, settlement) {
            Ok(_) => panic!("cancelled Tier 2 authority must not enter worker execution"),
            Err(mismatch) => mismatch,
        };
        assert_eq!(
            mismatch.settle(),
            IntegrityTier2OwnedWorkRejection::Cancelled
        );
        assert!(cancellation.is_cancelled());
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_worker_rejects_authority_spliced_to_another_state() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-spliced-state").await;
        let (foreign_state, foreign_root) = state_fixture("tier2-spliced-foreign-state", None);
        let IntegrityTier2OwnedWork {
            state: originating_state,
            ticket,
            settlement,
        } = work;
        let cancellation = settlement.cancellation();

        let mismatch = match IntegrityTier2OwnedWork::new(foreign_state.clone(), ticket, settlement)
        {
            Ok(_) => panic!("ticket and settlement must retain their originating state"),
            Err(mismatch) => mismatch,
        };
        assert_eq!(mismatch.settle(), IntegrityTier2OwnedWorkRejection::Refused);
        assert!(!cancellation.is_cancelled());

        drop(originating_state);
        close_fixture(state, root).await;
        close_fixture(foreign_state, foreign_root).await;
    }

    #[tokio::test]
    async fn tier_two_owned_result_seals_exact_opaque_assets_target_before_settlement() {
        let (state, root, work) =
            tier2_assets_owned_work_fixture("tier2-assets-exact-sealing").await;

        let result = work.spawn().join().await.expect("dedicated worker result");
        assert!(result.settlement.is_current());
        let sealed = result.seal_and_bind().await;
        assert!(sealed.settlement.is_current());
        assert_eq!(sealed.report.status, IntegrityTier2Status::Complete);
        assert!(sealed.report.repairable_observations.is_empty());
        let findings = sealed.findings.as_ref().expect("sealed Assets findings");
        assert_eq!(findings.len(), 1);
        let target = findings.repair_target().expect("selected Assets target");
        assert_eq!(target.system, StabilizationSystem::Execution);
        assert_eq!(target.kind, TargetKind::Artifact);
        assert_eq!(target.ownership, OwnershipClass::LauncherManaged);
        assert_eq!(target.id.len(), 79);
        assert!(target.id.starts_with("leaf-v2."));
        assert_eq!(
            sealed
                .report
                .facts
                .iter()
                .find(|fact| fact_field(fact, "observation") == Some("missing"))
                .and_then(|fact| fact.target.as_ref()),
            Some(target)
        );

        let (report, settlement) = sealed.settle_without_recovery();
        assert_eq!(settlement, IdleSweepSettlement::Authoritative);
        assert_eq!(report.status, IntegrityTier2Status::Complete);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_actual_confined_reader_cancels_during_a_multi_chunk_file() {
        let (state, root) = state_fixture("tier2-confined-cancellation", None);
        let library_root = root.join("private-library-root");
        let managed_parent = library_root.join("libraries/cancel");
        fs::create_dir_all(&managed_parent).expect("managed library parent");
        fs::write(managed_parent.join("large.jar"), vec![7_u8; 2 * 64 * 1024])
            .expect("managed library");
        let instance = state
            .instances()
            .insert_for_test("Tier two cancellation", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "cancel/large.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File {
                size: 2 * 64 * 1024,
            },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let idle_epoch = state.subscribe_integrity_idle().borrow().epoch();
        let producer = state
            .try_claim_producer()
            .expect("claim idle sweep producer");
        let reservation = state
            .try_reserve_idle_sweep(idle_epoch, producer)
            .expect("idle sweep reservation");
        let settlement = IdleSweepSettlementOwner::new(reservation);
        let ticket = state
            .mint_known_good_tier2_ticket(&settlement.authority(), &instance.id)
            .await
            .expect("Tier 2 ticket");
        let cancellation = settlement.cancellation();
        let cancel_from_thread = cancellation.clone();
        let canceller = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(2));
            cancel_from_thread.cancel();
        });
        let work = IntegrityTier2OwnedWork::new(state.clone(), ticket, settlement)
            .expect("matching Tier 2 ownership");

        let result = work.spawn().join().await.expect("dedicated worker result");
        canceller.join().expect("cancellation thread");
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Superseded);
        assert_eq!(report.status, IntegrityTier2Status::Cancelled);
        assert!(report.content_read_byte_count <= 64 * 1024);
        assert!(report.facts.is_empty());
        close_fixture(state, root).await;
    }

    #[test]
    fn tier_two_owned_boundary_contains_worker_panics_as_one_bounded_refusal() {
        let platform = ScriptedLowPriorityPlatform::successful();

        let report =
            run_integrity_tier2_owned(platform.clone(), || panic!("injected Tier 2 worker panic"));

        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(report.facts[0].kind, ExecutionFactKind::PrimitiveRefused);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("tier2_worker_unavailable")
        );
        assert_eq!(platform.events(), vec!["enter", "restore"]);
    }

    #[tokio::test]
    async fn tier_two_priority_enter_failure_refuses_without_sensing() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-enter-failure").await;
        let platform = ScriptedLowPriorityPlatform::enter_failure();

        let result = work
            .spawn_with_platform(platform.clone())
            .join()
            .await
            .expect("priority refusal result");
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Superseded);
        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.selected_entry_count, 0);
        assert_eq!(report.processed_entry_count, 0);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("tier2_low_priority_enter_failed")
        );
        assert_eq!(platform.events(), vec!["enter"]);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_thread_spawn_failure_recovers_untouched_work_and_unblocks_foreground() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-spawn-failure").await;
        let platform = ScriptedLowPriorityPlatform::successful();
        let spawner = RefusingTier2ThreadSpawner::default();

        let result = work
            .spawn_with_platform_and_spawner(platform.clone(), spawner.clone())
            .join()
            .await
            .expect("bounded spawn refusal result");
        assert!(result.settlement.is_current());
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Superseded);
        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.selected_entry_count, 0);
        assert_eq!(report.processed_entry_count, 0);
        assert_eq!(report.content_read_byte_count, 0);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("tier2_worker_unavailable")
        );
        assert!(platform.events().is_empty());
        assert_eq!(
            *spawner.names.lock().expect("thread names"),
            vec!["axial-tier-two-integrity"]
        );

        drop(
            tokio::time::timeout(
                Duration::from_secs(1),
                state
                    .register_integrity_foreground()
                    .expect("register foreground after refused spawn")
                    .wait_for_settlement(),
            )
            .await
            .expect("caller settlement releases refused worker ownership"),
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_priority_restore_failure_discards_facts_but_preserves_counters() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-restore-failure").await;
        let platform = ScriptedLowPriorityPlatform::restore_failure();

        let result = work
            .spawn_with_platform(platform.clone())
            .join()
            .await
            .expect("priority refusal result");
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Superseded);
        assert_eq!(report.status, IntegrityTier2Status::Refused);
        assert_eq!(report.selected_entry_count, 1);
        assert_eq!(report.processed_entry_count, 1);
        assert_eq!(report.hashed_entry_count, 1);
        assert_eq!(report.content_read_byte_count, 1);
        assert_eq!(report.facts.len(), 1);
        assert_eq!(
            fact_field(&report.facts[0], "observation"),
            Some("tier2_low_priority_restore_failed")
        );
        assert_eq!(platform.events(), vec!["enter", "restore", "restore"]);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_spawned_join_stays_pending_until_the_dedicated_worker_finishes() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-gated-join").await;
        let (gate, entered) = BlockingContentGate::new();
        let platform = ScriptedLowPriorityPlatform::gated(gate.clone());
        let worker = work.spawn_with_platform(platform.clone());
        let join = tokio::spawn(worker.join());

        entered
            .await
            .expect("dedicated worker enters priority scope");
        assert!(!join.is_finished());
        gate.release();
        let result = tokio::time::timeout(Duration::from_secs(5), join)
            .await
            .expect("dedicated worker finishes")
            .expect("join waiter")
            .expect("dedicated worker result");
        assert!(result.settlement.is_current());
        let (report, settlement) = settle_tier2_result(result).await;

        assert_eq!(settlement, IdleSweepSettlement::Authoritative);
        assert_eq!(report.status, IntegrityTier2Status::Complete);
        assert_eq!(platform.events(), vec!["enter", "restore"]);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn dropping_join_waiter_keeps_physical_sweep_ownership_until_thread_exit() {
        let (state, root, work) = tier2_owned_work_fixture("tier2-dropped-waiter").await;
        let (gate, entered) = BlockingContentGate::new();
        let worker = work.spawn_with_platform(ScriptedLowPriorityPlatform::gated(gate.clone()));
        let join = tokio::spawn(worker.join());
        entered
            .await
            .expect("dedicated worker enters priority scope");

        join.abort();
        assert!(
            join.await
                .expect_err("join waiter is aborted")
                .is_cancelled()
        );
        let foreground = state
            .register_integrity_foreground()
            .expect("register foreground against active sweep");
        let foreground_waiter = tokio::spawn(foreground.wait_for_settlement());
        tokio::task::yield_now().await;
        assert!(!foreground_waiter.is_finished());

        gate.release();
        drop(
            tokio::time::timeout(Duration::from_secs(5), foreground_waiter)
                .await
                .expect("foreground waits for physical worker exit")
                .expect("foreground waiter"),
        );
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_two_join_reports_a_bounded_closed_worker_error() {
        let (completion_tx, completion) = tokio::sync::oneshot::channel();
        drop(completion_tx);

        let error = IntegrityTier2BlockingWorker { completion }
            .join()
            .await
            .expect_err("closed worker completion must be bounded");

        assert_eq!(error, IntegrityTier2BlockingWorkerUnavailable);
        assert_eq!(
            error.to_string(),
            "Tier 2 dedicated worker stopped before returning sweep ownership"
        );
    }

    #[test]
    fn tier_two_primitive_is_move_only_and_has_no_direct_run_path() {
        fn assert_send<T: Send>() {}
        assert_send::<IntegrityTier2OwnedWork>();
        assert_send::<IntegrityTier2BlockingWorker>();

        let source = include_str!("integrity.rs");
        let owner = source
            .split("impl IntegrityTier2OwnedWork")
            .nth(1)
            .expect("Tier 2 owner implementation")
            .split("impl IntegrityTier2BlockingWorker")
            .next()
            .expect("Tier 2 owner body");
        let primitive = source
            .split("fn sense_integrity_tier2_owned")
            .nth(1)
            .expect("Tier 2 primitive")
            .split("fn run_integrity_tier2_with")
            .next()
            .expect("Tier 2 primitive body");
        assert!(!primitive.contains("spawn"));
        assert!(source.contains("Tier 2 work must be run by its blocking owner"));
        assert!(!source.contains(concat!("impl Clone for IntegrityTier2", "OwnedWork")));
        assert!(!owner.contains(concat!("pub(crate) fn ", "run(self)")));
        assert!(!owner.contains("spawn_blocking"));
        assert!(!source.contains(concat!("into_parts(self) -> (", "IdleSweepReservation")));
        assert!(source.contains("std::thread::Builder::new()"));
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
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 14,
                },
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
        let asset_index_ordinal = inventory
            .entries()
            .iter()
            .position(|entry| {
                entry.root() == &KnownGoodRoot::Assets
                    && entry.kind() == KnownGoodArtifactKind::AssetIndex
                    && entry.path().as_str() == "indexes/1.21.json"
            })
            .expect("canonical asset index ordinal");
        let inventory = inventory
            .with_test_standalone_leaf_repair_source(
                asset_index_ordinal,
                "https://example.invalid/1.21.json",
            )
            .expect("source-backed asset index");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
    async fn tier_one_seals_exact_repairable_findings_and_retains_live_authority() {
        let (state, root) = state_fixture("tier1-sealed-findings", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one sealed findings", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([
            entry(
                TestKnownGoodRoot::Versions,
                "1.21.5/1.21.5.jar",
                KnownGoodArtifactKind::ClientJar,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "private/missing.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "private/corrupt.jar",
                KnownGoodArtifactKind::NativeLibrary,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
            entry(
                TestKnownGoodRoot::Libraries,
                "private/permission.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            ),
        ])
        .expect("inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/private-corrupt.jar")
        .expect("corrupt repair source")
        .with_test_standalone_leaf_repair_source(1, "https://example.invalid/private-missing.jar")
        .expect("missing repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        let report = sense_integrity_tier1_with_reader_factory(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
            || {
                ScriptedContentReader::new([
                    (
                        "1.21.5/1.21.5.jar",
                        ScriptedContent::Hashed(NONZERO_SHA1, 7),
                    ),
                    (
                        "private/missing.jar",
                        ScriptedContent::Error(io::ErrorKind::NotFound),
                    ),
                    (
                        "private/corrupt.jar",
                        ScriptedContent::Hashed(NONZERO_SHA1, 7),
                    ),
                    (
                        "private/permission.jar",
                        ScriptedContent::Error(io::ErrorKind::PermissionDenied),
                    ),
                ])
            },
        )
        .await
        .expect("admitted Tier one report");

        assert_eq!(report.findings().len(), 2);
        assert!(!report.findings().is_empty());
        assert!(state.registered_artifact_findings_can_admit(report.findings()));
        let corrupt = RegisteredArtifactObservation::new(0, RegisteredArtifactCondition::Corrupt);
        let missing = RegisteredArtifactObservation::new(1, RegisteredArtifactCondition::Missing);
        let missing_target = report
            .findings()
            .target_for(missing)
            .expect("missing target");
        let corrupt_target = report
            .findings()
            .target_for(corrupt)
            .expect("corrupt target");
        assert_ne!(missing_target, corrupt_target);
        for target in [missing_target, corrupt_target] {
            assert_eq!(target.system, StabilizationSystem::Execution);
            assert_eq!(target.kind, TargetKind::Artifact);
            assert_eq!(target.ownership, OwnershipClass::LauncherManaged);
            assert_eq!(target.id.len(), 79);
            assert!(target.id.starts_with("leaf-v2."));
        }
        let fact_for = |observation| {
            report
                .facts
                .iter()
                .find(|fact| fact_field(fact, "observation") == Some(observation))
                .unwrap_or_else(|| panic!("missing {observation} fact"))
        };
        assert_eq!(fact_for("missing").target.as_ref(), Some(missing_target));
        assert_eq!(
            report
                .facts
                .iter()
                .find(|fact| {
                    fact_field(fact, "entry_ordinal") == Some("0")
                        && fact_field(fact, "observation") == Some("hash_mismatch")
                })
                .and_then(|fact| fact.target.as_ref()),
            Some(corrupt_target)
        );
        assert_eq!(
            fact_for("content_permission_denied")
                .target
                .as_ref()
                .map(|target| target.id.as_str()),
            Some("known_good_libraries_library_2")
        );
        assert_eq!(
            report
                .facts
                .iter()
                .find(|fact| {
                    fact_field(fact, "inventory_root") == Some("versions")
                        && fact_field(fact, "observation") == Some("hash_mismatch")
                })
                .and_then(|fact| fact.target.as_ref())
                .map(|target| target.id.as_str()),
            Some("known_good_versions_client_jar_3")
        );
        assert_eq!(
            report
                .findings()
                .observations_for_test()
                .map(|(observation, _)| {
                    (observation.inventory_ordinal(), observation.condition())
                })
                .collect::<Vec<_>>(),
            vec![
                (0, RegisteredArtifactCondition::Corrupt),
                (1, RegisteredArtifactCondition::Missing),
            ]
        );

        drop(lifecycle);
        drop(foreground);
        assert!(!state.subscribe_integrity_idle().borrow().is_stably_idle());
        let mut lifecycle_wait = Box::pin(state.acquire_instance_lifecycle(&instance.id));
        assert!(
            tokio::time::timeout(Duration::from_millis(50), &mut lifecycle_wait)
                .await
                .is_err(),
            "sealed findings must retain exact lifecycle authority"
        );
        drop(report);
        let released = tokio::time::timeout(Duration::from_secs(2), &mut lifecycle_wait)
            .await
            .expect("findings release lifecycle authority");
        drop(released);
        drop(lifecycle_wait);
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn registered_artifact_authorization_is_managed_exact_and_source_backed() {
        let (state, root) = state_fixture("registered-artifact-authorization", None);
        let instance = state
            .instances()
            .insert_for_test("Registered artifact authorization", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "exact/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::Sha1 {
                digest: ZERO_SHA1.to_string(),
                size: 7,
            },
        )])
        .expect("inventory")
        .with_test_standalone_leaf_repair_source(0, "https://example.invalid/exact-library.jar")
        .expect("repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        for mode in [GuardianMode::Custom, GuardianMode::Disabled] {
            let report = sense_integrity_tier1(
                &state,
                &foreground,
                &lifecycle,
                &root.join("private-library-root"),
            )
            .await
            .expect("Tier one report");
            let (_, findings) = report.into_parts();
            let target = findings
                .repair_target()
                .expect("exact repair target")
                .clone();
            assert!(matches!(
                findings.authorize_repair(&registered_artifact_decision(target, mode)),
                Err(RegisteredArtifactRepairAuthorizationRejection::NonManagedRepair)
            ));
        }

        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
        )
        .await
        .expect("managed Tier one report");
        let (_, findings) = report.into_parts();
        assert!(findings.repair_target().is_some());
        let selected = findings.repair_target().expect("selected target").clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(
                selected,
                GuardianMode::Managed,
            ))
            .expect("managed authorization");
        drop(authorization);

        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "exact/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::Sha1 {
                digest: ZERO_SHA1.to_string(),
                size: 7,
            },
        )])
        .expect("inventory without repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
        )
        .await
        .expect("source-less Tier one report");
        let (_, findings) = report.into_parts();
        assert!(findings.repair_target().is_none());
        assert!(findings.is_empty());

        drop(lifecycle);
        drop(foreground);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn assets_index_object_and_zero_byte_sources_admit_exact_missing_leaf_repairs() {
        let object_bytes = b"source-backed-asset-object";
        let object_digest = format!("{:x}", Sha1::digest(object_bytes));
        let empty_digest = format!("{:x}", Sha1::digest([]));
        let cases = [
            (
                "index",
                "indexes/source-backed.json".to_string(),
                KnownGoodArtifactKind::AssetIndex,
                format!("{:x}", Sha1::digest(b"source-backed-index")),
                b"source-backed-index".len() as u64,
                "https://example.invalid/source-backed.json".to_string(),
            ),
            (
                "object",
                format!("objects/{}/{}", &object_digest[..2], object_digest),
                KnownGoodArtifactKind::AssetObject,
                object_digest.clone(),
                object_bytes.len() as u64,
                format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &object_digest[..2],
                    object_digest
                ),
            ),
            (
                "zero-object",
                format!("objects/{}/{}", &empty_digest[..2], empty_digest),
                KnownGoodArtifactKind::AssetObject,
                empty_digest.clone(),
                0,
                format!(
                    "https://resources.download.minecraft.net/{}/{}",
                    &empty_digest[..2],
                    empty_digest
                ),
            ),
        ];

        for (label, path, kind, digest, size, provider_url) in cases {
            let (state, root) = state_fixture(&format!("assets-leaf-{label}"), None);
            let instance = state
                .instances()
                .insert_for_test("Assets registered leaf", "1.21.5")
                .expect("instance");
            let inventory = KnownGoodInventory::from_test_entries([entry(
                TestKnownGoodRoot::Assets,
                &path,
                kind,
                TestKnownGoodIntegrity::Sha1 { digest, size },
            )])
            .expect("Assets leaf inventory")
            .with_test_standalone_leaf_repair_source(0, &provider_url)
            .expect("Assets leaf source");
            state.activate_known_good_inventory_for_test(&instance.id, inventory);
            let destination = root.join("private-library-root/assets").join(&path);
            fs::create_dir_all(
                destination
                    .parent()
                    .expect("missing Assets leaf parent path"),
            )
            .unwrap_or_else(|error| panic!("create missing Assets {label} parent: {error}"));
            assert!(!destination.exists(), "Assets {label} leaf must be missing");
            let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
            let findings = seal_registered_leaf_finding(
                &state,
                &lifecycle,
                &root.join("private-library-root"),
                0,
                RegisteredArtifactCondition::Missing,
            )
            .await;
            let candidate = findings
                .repair_candidate()
                .expect("Assets repair candidate");
            assert_eq!(
                candidate.domain(),
                crate::guardian::GuardianDomain::Download
            );
            assert_eq!(candidate.target().id.len(), 79);
            let provenance = candidate
                .target()
                .id
                .strip_prefix("leaf-v2.")
                .expect("truthful target prefix");
            assert_eq!(provenance.len(), 71);
            let segments = provenance.split('.').collect::<Vec<_>>();
            assert_eq!(segments.len(), 8);
            assert!(segments.iter().all(|segment| {
                segment.len() == 8
                    && segment
                        .bytes()
                        .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
            }));
            let target = candidate.target().clone();
            let authorization = findings
                .authorize_repair(&registered_artifact_decision(target, GuardianMode::Managed))
                .expect("Assets leaf authorization");
            let admission = state
                .admit_registered_artifact_repair(
                    authorization,
                    OperationId::new(format!("assets-leaf-{label}")),
                    chrono::Duration::minutes(15),
                )
                .await
                .expect("Assets leaf admission");

            assert_eq!(
                admission.effect(),
                RegisteredArtifactRepairEffect::DownloadMissing
            );
            assert_eq!(
                admission.attempt().domain(),
                crate::guardian::GuardianDomain::Download
            );
            assert_eq!(
                admission.attempt().component(),
                ReconciliationComponent::Assets
            );
            assert_eq!(admission.download_contract().2, size);

            drop((admission, lifecycle));
            close_fixture(state, root).await;
        }
    }

    #[tokio::test]
    async fn cancelled_sweep_leaf_settles_failure_but_cannot_admit_component() {
        let (state, root, reservation, admission) =
            corrupt_assets_sweep_leaf_fixture("sweep-leaf-cancelled").await;
        let cancellation = reservation.cancellation();
        cancellation.cancel();
        assert!(admission.evidence_is_live());
        let failure =
            match execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("cancelled admitted leaf settles")
            {
                GuardianArtifactRepairSettlement::Failed(failure) => failure,
                GuardianArtifactRepairSettlement::Completed(_) => {
                    panic!("corrupt Assets leaf must fail to the component rung")
                }
            };
        assert_eq!(
            failure.outcome().status(),
            GuardianArtifactRepairStatus::Failed
        );
        assert!(matches!(
            state
                .admit_registered_artifact_component_rebuild(
                    failure.into_continuation(),
                    OperationId::new("sweep-leaf-cancelled-component"),
                    chrono::Duration::minutes(30),
                )
                .await,
            Err(crate::state::ReconciliationEvidenceRejection::IncarnationMismatch)
        ));
        reservation.settle(IdleSweepTerminal::Cancelled);

        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn cancelled_sweep_component_finishes_after_admission() {
        let (state, root, reservation, admission) =
            corrupt_assets_sweep_leaf_fixture("sweep-component-cancelled").await;
        let failure =
            match execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("Assets leaf settles")
            {
                GuardianArtifactRepairSettlement::Failed(failure) => failure,
                GuardianArtifactRepairSettlement::Completed(_) => {
                    panic!("corrupt Assets leaf must fail to the component rung")
                }
            };
        let component = state
            .admit_registered_artifact_component_rebuild(
                failure.into_continuation(),
                OperationId::new("sweep-component-cancelled-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admitted while sweep is current");

        reservation.cancellation().cancel();
        assert!(component.assets_effect().is_ok());
        let outcome = execute_failed_managed_assets_component_rebuild_for_test(component)
            .await
            .expect("admitted component settles while sweep remains active");
        assert_eq!(
            outcome.status,
            crate::guardian::GuardianComponentRebuildStatus::Failed
        );
        reservation.settle(IdleSweepTerminal::Cancelled);

        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn settled_sweep_revokes_an_admitted_component_before_effect() {
        let (state, root, reservation, admission) =
            corrupt_assets_sweep_leaf_fixture("sweep-component-settled").await;
        let failure =
            match execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("Assets leaf settles")
            {
                GuardianArtifactRepairSettlement::Failed(failure) => failure,
                GuardianArtifactRepairSettlement::Completed(_) => {
                    panic!("corrupt Assets leaf must fail to the component rung")
                }
            };
        let component = state
            .admit_registered_artifact_component_rebuild(
                failure.into_continuation(),
                OperationId::new("sweep-component-settled-rebuild"),
                chrono::Duration::minutes(30),
            )
            .await
            .expect("component admitted while sweep is current");

        reservation.cancellation().cancel();
        assert!(component.assets_effect().is_ok());
        assert!(component.failed_terminal().is_ok());
        reservation.settle(IdleSweepTerminal::Cancelled);
        assert_eq!(
            component.assets_effect().err(),
            Some(crate::state::ReconciliationEvidenceRejection::IncarnationMismatch)
        );
        assert_eq!(
            component.failed_terminal().err(),
            Some(crate::state::ReconciliationEvidenceRejection::IncarnationMismatch)
        );

        drop(component);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn corrupt_asset_requires_component_rebuild_without_mutation_or_network() {
        let expected = b"expected-asset-index";
        let corrupt = vec![b'x'; expected.len()];
        let (state, root) = state_fixture("corrupt-asset-component", None);
        let instance = state
            .instances()
            .insert_for_test("Corrupt Assets registered leaf", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Assets,
            "indexes/corrupt.json",
            KnownGoodArtifactKind::AssetIndex,
            TestKnownGoodIntegrity::Sha1 {
                digest: format!("{:x}", Sha1::digest(expected)),
                size: expected.len() as u64,
            },
        )])
        .expect("corrupt Assets inventory")
        .with_test_standalone_leaf_repair_source(0, "http://127.0.0.1:0/unreachable")
        .expect("corrupt Assets source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let destination = root.join("private-library-root/assets/indexes/corrupt.json");
        fs::create_dir_all(destination.parent().expect("Assets index parent"))
            .expect("Assets index parent");
        fs::write(&destination, &corrupt).expect("corrupt Assets index");
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let findings = seal_registered_leaf_finding(
            &state,
            &lifecycle,
            &root.join("private-library-root"),
            0,
            RegisteredArtifactCondition::Corrupt,
        )
        .await;
        let target = findings
            .repair_target()
            .expect("Assets repair target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(
                target.clone(),
                GuardianMode::Managed,
            ))
            .expect("corrupt Assets authorization");
        let operation_id = OperationId::new("corrupt-asset-component");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("corrupt Assets admission");
        assert_eq!(
            admission.effect(),
            RegisteredArtifactRepairEffect::ComponentRebuildRequired
        );
        let settlement =
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("corrupt Assets leaf settlement");
        let failure = match settlement {
            GuardianArtifactRepairSettlement::Failed(failure) => failure,
            GuardianArtifactRepairSettlement::Completed(_) => {
                panic!("corrupt Assets leaf must require component rebuild")
            }
        };

        assert_eq!(
            failure.outcome().status(),
            GuardianArtifactRepairStatus::Failed
        );
        assert_eq!(
            fs::read(&destination).expect("unchanged Assets index"),
            corrupt
        );
        assert_eq!(
            fs::read_dir(destination.parent().expect("Assets index parent"))
                .expect("list Assets indexes")
                .count(),
            1
        );
        let journal = state
            .journals()
            .get(&operation_id)
            .expect("Assets leaf journal");
        assert!(journal.planned_steps.iter().all(|step| {
            !step.step_id.contains("quarantine")
                && !step.step_id.contains("download")
                && !step.step_id.contains("promote")
        }));
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("Assets failure step")
                .step_id,
            "require_registered_artifact_component_rebuild"
        );
        let terminal = journal
            .reconciliation_terminal()
            .expect("Assets failed terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Failed);
        assert_eq!(terminal.component(), ReconciliationComponent::Assets);
        assert_eq!(terminal.domain(), crate::guardian::GuardianDomain::Download);
        assert_eq!(terminal.target(), &target);
        assert!(terminal.quarantined_target().is_none());
        assert!(
            state
                .failure_memory()
                .get(&crate::state::reconciliation_attempt_key(
                    terminal.attempt()
                ))
                .is_some()
        );
        let component = state
            .admit_registered_artifact_component_rebuild(
                failure.into_continuation(),
                OperationId::new("corrupt-asset-component-rebuild"),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("adjacent Assets component admission");
        assert_eq!(
            component.attempt().component(),
            ReconciliationComponent::Assets
        );
        assert_eq!(component.attempt().target(), &target);

        drop((component, lifecycle));
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn missing_asset_index_downloads_and_settles_exact_assets_terminal() {
        let body = b"missing-asset-index".to_vec();
        let digest = format!("{:x}", Sha1::digest(&body));
        let server = RegisteredArtifactServer::start(body.clone());
        let (state, root) = state_fixture("missing-asset-index", None);
        let instance = state
            .instances()
            .insert_for_test("Missing Assets registered leaf", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Assets,
            "indexes/missing.json",
            KnownGoodArtifactKind::AssetIndex,
            TestKnownGoodIntegrity::Sha1 {
                digest,
                size: body.len() as u64,
            },
        )])
        .expect("missing Assets inventory")
        .with_test_standalone_leaf_repair_source(0, &server.url)
        .expect("missing Assets source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let destination = root.join("private-library-root/assets/indexes/missing.json");
        fs::create_dir_all(destination.parent().expect("missing Assets index parent"))
            .expect("create missing Assets index parent");
        assert!(!destination.exists(), "Assets index leaf must be missing");
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let findings = seal_registered_leaf_finding(
            &state,
            &lifecycle,
            &root.join("private-library-root"),
            0,
            RegisteredArtifactCondition::Missing,
        )
        .await;
        let target = findings
            .repair_target()
            .expect("Assets repair target")
            .clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(
                target.clone(),
                GuardianMode::Managed,
            ))
            .expect("missing Assets authorization");
        let operation_id = OperationId::new("missing-asset-index");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("missing Assets admission");
        let settlement =
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("missing Assets repair");

        assert_eq!(
            settlement.outcome().status(),
            GuardianArtifactRepairStatus::Repaired
        );
        assert_eq!(fs::read(destination).expect("repaired Assets index"), body);
        let terminal = state
            .journals()
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("successful Assets terminal");
        assert_eq!(terminal.outcome(), ReconciliationTerminalOutcome::Succeeded);
        assert_eq!(terminal.component(), ReconciliationComponent::Assets);
        assert_eq!(terminal.domain(), crate::guardian::GuardianDomain::Download);
        assert_eq!(terminal.target(), &target);
        assert!(terminal.quarantined_target().is_none());

        drop(lifecycle);
        server.close();
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn registered_artifact_missing_and_corrupt_repairs_settle_exact_registered_terminal() {
        for (label, corrupt) in [("missing", false), ("corrupt", true)] {
            let body = format!("registered-{label}-artifact").into_bytes();
            let digest = format!("{:x}", Sha1::digest(&body));
            let server = RegisteredArtifactServer::start(body.clone());
            let (state, root) = state_fixture(&format!("registered-artifact-{label}"), None);
            let instance = state
                .instances()
                .insert_for_test("Registered artifact repair", "1.21.5")
                .expect("instance");
            let inventory = KnownGoodInventory::from_test_entries([entry(
                TestKnownGoodRoot::Libraries,
                "exact/library.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::Sha1 {
                    digest,
                    size: body.len() as u64,
                },
            )])
            .expect("inventory")
            .with_test_standalone_leaf_repair_source(0, &server.url)
            .expect("repair source");
            state.activate_known_good_inventory_for_test(&instance.id, inventory);
            let destination = root.join("private-library-root/libraries/exact/library.jar");
            fs::create_dir_all(destination.parent().expect("library parent"))
                .expect("library parent");
            if corrupt {
                fs::write(&destination, vec![b'x'; body.len()]).expect("corrupt library");
            }
            let foreground = test_integrity_foreground(&state).await;
            let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
            let report = sense_integrity_tier1(
                &state,
                &foreground,
                &lifecycle,
                &root.join("private-library-root"),
            )
            .await
            .expect("Tier one report");
            let (_, findings) = report.into_parts();
            let target = findings.repair_target().expect("repair target").clone();
            let authorization = findings
                .authorize_repair(&registered_artifact_decision(
                    target.clone(),
                    GuardianMode::Managed,
                ))
                .expect("repair authorization");
            let operation_id = OperationId::new(format!("registered-artifact-{label}"));
            let admission = state
                .admit_registered_artifact_repair(
                    authorization,
                    operation_id.clone(),
                    chrono::Duration::minutes(15),
                )
                .await
                .expect("repair admission");
            let settlement =
                execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                    .await
                    .expect("registered artifact repair");

            assert_eq!(
                settlement.outcome().status(),
                GuardianArtifactRepairStatus::Repaired
            );
            assert_eq!(fs::read(&destination).expect("repaired artifact"), body);
            let journal = state.journals().get(&operation_id).expect("repair journal");
            let terminal = journal.reconciliation_terminal().expect("typed terminal");
            assert_eq!(terminal.rung(), ReconciliationRung::RepairArtifact);
            assert_eq!(terminal.component(), ReconciliationComponent::Libraries);
            assert_eq!(terminal.target(), &target);
            assert!(matches!(
                terminal.scope(),
                ReconciliationScope::RegisteredInstance { instance_id, .. }
                    if instance_id == &instance.id
            ));
            assert_eq!(terminal.quarantined_target().is_some(), corrupt);

            fs::write(&destination, vec![b'y'; body.len()]).expect("repeat corruption");
            let report = sense_integrity_tier1(
                &state,
                &foreground,
                &lifecycle,
                &root.join("private-library-root"),
            )
            .await
            .expect("repeat Tier one report");
            let (_, findings) = report.into_parts();
            let target = findings.repair_target().expect("repeat target").clone();
            let authorization = findings
                .authorize_repair(&registered_artifact_decision(target, GuardianMode::Managed))
                .expect("repeat authorization");
            assert_eq!(
                state
                    .admit_registered_artifact_repair(
                        authorization,
                        OperationId::new(format!("registered-artifact-{label}-suppressed")),
                        chrono::Duration::minutes(15),
                    )
                    .await
                    .err(),
                Some(crate::state::ReconciliationEvidenceRejection::SuppressedPriorAttempt)
            );

            drop(lifecycle);
            drop(foreground);
            server.close();
            close_fixture(state, root).await;
        }
    }

    #[tokio::test]
    async fn registered_artifact_repaired_while_waiting_settles_without_network_or_quarantine() {
        let body = b"already-repaired-registered-artifact".to_vec();
        let digest = format!("{:x}", Sha1::digest(&body));
        let (state, root) = state_fixture("registered-artifact-already-repaired", None);
        let instance = state
            .instances()
            .insert_for_test("Registered artifact already repaired", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "exact/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::Sha1 {
                digest,
                size: body.len() as u64,
            },
        )])
        .expect("inventory")
        .with_test_standalone_leaf_repair_source(0, "http://127.0.0.1:0/unreachable")
        .expect("repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let destination = root.join("private-library-root/libraries/exact/library.jar");
        fs::create_dir_all(destination.parent().expect("library parent")).expect("library parent");
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
        )
        .await
        .expect("Tier one report");
        let (_, findings) = report.into_parts();
        let target = findings.repair_target().expect("repair target").clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(
                target.clone(),
                GuardianMode::Managed,
            ))
            .expect("repair authorization");
        let operation_id = OperationId::new("registered-artifact-already-repaired");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("repair admission");

        fs::write(&destination, &body).expect("shared repair completed while waiting");
        let settlement =
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("already repaired outcome");

        assert_eq!(
            settlement.outcome().status(),
            GuardianArtifactRepairStatus::Repaired
        );
        let journal = state.journals().get(&operation_id).expect("repair journal");
        assert_eq!(
            journal
                .completed_steps
                .last()
                .expect("completed no-op")
                .step_id,
            "registered_artifact_already_exact"
        );
        let terminal = journal.reconciliation_terminal().expect("typed terminal");
        assert_eq!(terminal.target(), &target);
        assert!(terminal.quarantined_target().is_none());

        drop(lifecycle);
        drop(foreground);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn registered_artifact_admission_revalidates_inventory_after_config_wait() {
        let (state, root) = state_fixture("registered-artifact-config-wait", None);
        let instance = state
            .instances()
            .insert_for_test("Registered artifact config wait", "1.21.5")
            .expect("instance");
        let inventory = || {
            KnownGoodInventory::from_test_entries([entry(
                TestKnownGoodRoot::Libraries,
                "exact/library.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::Sha1 {
                    digest: ZERO_SHA1.to_string(),
                    size: 7,
                },
            )])
            .expect("inventory")
            .with_test_standalone_leaf_repair_source(0, "https://example.invalid/exact-library.jar")
            .expect("repair source")
        };
        state.activate_known_good_inventory_for_test(&instance.id, inventory());
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
        )
        .await
        .expect("Tier one report");
        let (_, findings) = report.into_parts();
        let target = findings.repair_target().expect("repair target").clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(target, GuardianMode::Managed))
            .expect("repair authorization");
        let config_gate = state
            .config()
            .acquire_mutation()
            .await
            .expect("config gate");
        let mut admission = Box::pin(state.admit_registered_artifact_repair(
            authorization,
            OperationId::new("registered-artifact-config-wait"),
            chrono::Duration::minutes(15),
        ));
        assert!(
            tokio::time::timeout(Duration::from_millis(30), &mut admission)
                .await
                .is_err()
        );
        state.activate_known_good_inventory_for_test(&instance.id, inventory());
        drop(config_gate);
        assert_eq!(
            admission.await.err(),
            Some(crate::state::ReconciliationEvidenceRejection::IncarnationMismatch)
        );

        drop(lifecycle);
        drop(foreground);
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn registered_artifact_hash_failure_records_failed_rung_one_terminal() {
        let expected = b"expected-registered-artifact".to_vec();
        let served = b"rejected-registered-artifact".to_vec();
        assert_eq!(expected.len(), served.len());
        let server = RegisteredArtifactServer::start(served);
        let (state, root) = state_fixture("registered-artifact-hash-failure", None);
        let instance = state
            .instances()
            .insert_for_test("Registered artifact hash failure", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "exact/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::Sha1 {
                digest: format!("{:x}", Sha1::digest(&expected)),
                size: expected.len() as u64,
            },
        )])
        .expect("inventory")
        .with_test_standalone_leaf_repair_source(0, &server.url)
        .expect("repair source");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        fs::create_dir_all(root.join("private-library-root/libraries/exact"))
            .expect("library parent");
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let report = sense_integrity_tier1(
            &state,
            &foreground,
            &lifecycle,
            &root.join("private-library-root"),
        )
        .await
        .expect("Tier one report");
        let (_, findings) = report.into_parts();
        let target = findings.repair_target().expect("repair target").clone();
        let authorization = findings
            .authorize_repair(&registered_artifact_decision(
                target.clone(),
                GuardianMode::Managed,
            ))
            .expect("repair authorization");
        let operation_id = OperationId::new("registered-artifact-hash-failure");
        let admission = state
            .admit_registered_artifact_repair(
                authorization,
                operation_id.clone(),
                chrono::Duration::minutes(15),
            )
            .await
            .expect("repair admission");
        let settlement =
            execute_registered_guardian_artifact_repair(admission, &reqwest::Client::new())
                .await
                .expect("failed repair outcome");

        assert_eq!(
            settlement.outcome().status(),
            GuardianArtifactRepairStatus::Failed
        );
        let terminal = state
            .journals()
            .get(&operation_id)
            .and_then(|journal| journal.reconciliation_terminal().cloned())
            .expect("failed typed terminal");
        assert_eq!(terminal.target(), &target);
        assert_eq!(
            terminal.outcome(),
            crate::state::contracts::ReconciliationTerminalOutcome::Failed
        );
        assert!(
            state
                .failure_memory()
                .get(&crate::state::reconciliation_attempt_key(
                    terminal.attempt()
                ))
                .is_some()
        );

        match settlement {
            GuardianArtifactRepairSettlement::Failed(failure) => drop(failure),
            GuardianArtifactRepairSettlement::Completed(_) => {
                panic!("hash failure must return a typed failed settlement")
            }
        }

        drop(lifecycle);
        drop(foreground);
        server.close();
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_rejects_inventory_replacement_before_findings_are_sealed() {
        let (state, root) = state_fixture("tier1-finding-inventory-drift", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one inventory drift", "1.21.5")
            .expect("instance");
        let inventory = || {
            KnownGoodInventory::from_test_entries([entry(
                TestKnownGoodRoot::Libraries,
                "drift/library.jar",
                KnownGoodArtifactKind::Library,
                TestKnownGoodIntegrity::File { size: 7 },
            )])
            .expect("inventory")
        };
        state.activate_known_good_inventory_for_test(&instance.id, inventory());
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let (gate, started) = BlockingContentGate::new();
        let sensing_gate = gate.clone();
        let sensing_state = state.clone();
        let sensing_root = root.join("private-library-root");
        let sensing = tokio::spawn(async move {
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &foreground,
                &lifecycle,
                &sensing_root,
                move || BlockingContentReader { gate: sensing_gate },
            )
            .await
        });

        started.await.expect("Tier one worker started");
        state.activate_known_good_inventory_for_test(&instance.id, inventory());
        gate.release();
        assert!(matches!(
            sensing.await.expect("Tier one task"),
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
        close_fixture(state, root).await;
    }

    #[tokio::test]
    async fn tier_one_rejects_instance_incarnation_drift_before_findings_are_sealed() {
        let (state, root) = state_fixture("tier1-finding-incarnation-drift", None);
        let instance = state
            .instances()
            .insert_for_test("Tier one incarnation drift", "1.21.5")
            .expect("instance");
        let inventory = KnownGoodInventory::from_test_entries([entry(
            TestKnownGoodRoot::Libraries,
            "drift/library.jar",
            KnownGoodArtifactKind::Library,
            TestKnownGoodIntegrity::File { size: 7 },
        )])
        .expect("inventory");
        state.activate_known_good_inventory_for_test(&instance.id, inventory);
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;
        let (gate, started) = BlockingContentGate::new();
        let sensing_gate = gate.clone();
        let sensing_state = state.clone();
        let sensing_root = root.join("private-library-root");
        let sensing = tokio::spawn(async move {
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &foreground,
                &lifecycle,
                &sensing_root,
                move || BlockingContentReader { gate: sensing_gate },
            )
            .await
        });

        started.await.expect("Tier one worker started");
        let mut replacement = instance.clone();
        replacement.version_id = "1.21.6".to_string();
        state
            .instances()
            .replace_for_test(replacement)
            .expect("replace instance incarnation");
        gate.release();
        assert!(matches!(
            sensing.await.expect("Tier one task"),
            Err(KnownGoodVerificationUnavailable::LiveAuthorityUnavailable)
        ));
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
    async fn aborted_tier_one_waiter_retains_foreground_and_lifecycle_until_physical_completion() {
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
            let foreground = test_integrity_foreground(&sensing_state).await;
            let lifecycle = sensing_state
                .acquire_instance_lifecycle(&sensing_instance_id)
                .await;
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &foreground,
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
        assert!(
            !state.subscribe_integrity_idle().borrow().is_stably_idle(),
            "blocking Tier one worker must retain foreground authority"
        );

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
        assert!(state.subscribe_integrity_idle().borrow().is_stably_idle());
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
            let foreground = test_integrity_foreground(&sensing_state).await;
            let lifecycle = sensing_state
                .acquire_instance_lifecycle(&sensing_instance_id)
                .await;
            sense_integrity_tier1_with_reader_factory(
                &sensing_state,
                &foreground,
                &lifecycle,
                &sensing_library_root,
                move || BlockingFilesystemContentReader {
                    inner: FilesystemIntegrityReader::default(),
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let lease =
            mint_test_verification_lease(&state, &lifecycle, &root.join("private-library-root"))
                .await;
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
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        let report = sense_integrity_tier0(
            &state,
            &foreground,
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
        drop(foreground);
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
        let foreground = test_integrity_foreground(&state).await;
        let lifecycle = state.acquire_instance_lifecycle(&instance.id).await;

        let warmup_report = sense_integrity_tier0(
            &state,
            &foreground,
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
                &foreground,
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
        drop(foreground);
        drop(lifecycle);
        close_fixture(state, root).await;
    }
}
