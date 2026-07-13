//! Metadata-only verification of exact live launcher-owned inventory authority.

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
    LaunchTier0RuntimeSelection, known_good_entry_path, known_good_link_target_matches,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
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

trait MetadataReader {
    fn symlink_metadata(&self, path: &KnownGoodPhysicalPath) -> io::Result<MetadataObservation>;
    fn read_link(&self, path: &KnownGoodPhysicalPath) -> io::Result<PathBuf>;
    fn revalidate(&self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(unix)]
mod confined_fs {
    use super::{MetadataKind, MetadataObservation};
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
            Ok(())
        }
    }
}

#[cfg(windows)]
mod confined_fs {
    use super::{MetadataKind, MetadataObservation};
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
        CloseHandle, HANDLE, OBJ_CASE_INSENSITIVE, RtlNtStatusToDosError, UNICODE_STRING,
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
                    FILE_READ_ATTRIBUTES | SYNCHRONIZE,
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
    lease: &KnownGoodVerificationLease<'_>,
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
    let root = entry.root().stable_id();
    let artifact_kind = entry.kind().stable_id();
    let mut fact = ExecutionFact {
        operation_id: None,
        kind,
        target: Some(TargetDescriptor::new(
            StabilizationSystem::Execution,
            if matches!(entry.root(), KnownGoodRoot::ManagedRuntime { .. }) {
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
    if let KnownGoodRoot::ManagedRuntime { component } = entry.root() {
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
    use std::sync::Arc;
    use std::sync::Mutex;
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
