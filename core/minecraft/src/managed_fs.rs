use crate::artifact_path::{ArtifactRelativePath, validate_artifact_path_segment};
use crate::known_good_libraries::RetainedInstallerLibrarySource;
use crate::loaders::types::LoaderError;
use sha1::{Digest as _, Sha1};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use sysinfo::{Pid, ProcessRefreshKind, ProcessesToUpdate, System};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

const TEMP_PREFIX: &str = ".axial-loader-tmp-";
const MAX_TEMP_SWEEP_ENTRIES: usize = 128;
const MAX_MANAGED_DIRECTORY_ENTRIES: usize = 4096;
const MAX_MANAGED_READ_BYTES: u64 = 512 << 20;
const MAX_MANAGED_TREE_ENTRIES: usize = MAX_MANAGED_DIRECTORY_ENTRIES;
const MAX_MANAGED_TREE_DEPTH: usize = 16;
const MAX_MANAGED_TREE_FILE_BYTES: u64 = 128 << 20;
const MAX_MANAGED_TREE_TOTAL_BYTES: u64 = 512 << 20;
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TEMPS: OnceLock<Mutex<HashSet<ActiveTempKey>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct ManagedDir {
    inner: Arc<ManagedDirInner>,
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub(crate) struct ManagedDirectoryIdentity(platform::DirectoryIdentity);

impl ManagedDirectoryIdentity {
    pub(crate) fn persistent_binding(self) -> String {
        platform::directory_identity_binding(self.0)
    }
}

pub(crate) struct ManagedPersistentFile {
    directory: ManagedDir,
    name: OsString,
    identity: platform::FileIdentity,
    file: std::fs::File,
}

pub(crate) struct ManagedFileGuard {
    identity: platform::FileIdentity,
    file: std::fs::File,
    size: u64,
}

pub(crate) struct MaterializedInstallerLibrary {
    source: RetainedInstallerLibrarySource,
    destination: PathBuf,
}

impl MaterializedInstallerLibrary {
    pub(crate) fn into_parts(self) -> (RetainedInstallerLibrarySource, PathBuf) {
        (self.source, self.destination)
    }
}

impl std::fmt::Debug for ManagedDir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedDir")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
    }
}

impl std::fmt::Debug for ManagedPersistentFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedPersistentFile")
            .finish_non_exhaustive()
    }
}

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
}

struct ManagedDirInner {
    path: PathBuf,
    identity: platform::DirectoryIdentity,
    handle: platform::DirectoryHandle,
    binding: DirectoryBinding,
}

enum DirectoryBinding {
    Root,
    Child {
        parent: Arc<ManagedDirInner>,
        name: OsString,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EntryKind {
    File,
    Directory,
    Link,
    #[cfg(unix)]
    Other,
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
    files: BTreeMap<ArtifactRelativePath, ManagedFileFact>,
    directories: BTreeSet<ArtifactRelativePath>,
}

impl ManagedTreeSnapshot {
    pub(crate) fn files(&self) -> &BTreeMap<ArtifactRelativePath, ManagedFileFact> {
        &self.files
    }

    pub(crate) fn directories(&self) -> &BTreeSet<ArtifactRelativePath> {
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
    added_files: BTreeMap<ArtifactRelativePath, ManagedFileFact>,
    removed_files: BTreeMap<ArtifactRelativePath, ManagedFileFact>,
    modified_files: BTreeMap<ArtifactRelativePath, (ManagedFileFact, ManagedFileFact)>,
    added_directories: BTreeSet<ArtifactRelativePath>,
    removed_directories: BTreeSet<ArtifactRelativePath>,
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

    pub(crate) fn added_files(&self) -> &BTreeMap<ArtifactRelativePath, ManagedFileFact> {
        &self.added_files
    }

    pub(crate) fn removed_files(&self) -> &BTreeMap<ArtifactRelativePath, ManagedFileFact> {
        &self.removed_files
    }

    pub(crate) fn modified_files(
        &self,
    ) -> &BTreeMap<ArtifactRelativePath, (ManagedFileFact, ManagedFileFact)> {
        &self.modified_files
    }

    pub(crate) fn added_directories(&self) -> &BTreeSet<ArtifactRelativePath> {
        &self.added_directories
    }

    pub(crate) fn removed_directories(&self) -> &BTreeSet<ArtifactRelativePath> {
        &self.removed_directories
    }
}

struct TreeCaptureBudget {
    remaining_entries: usize,
    remaining_bytes: u64,
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActiveTempKey {
    directory: platform::DirectoryIdentity,
    name: OsString,
}

struct ActiveTemp {
    key: ActiveTempKey,
}

struct PendingTemp {
    directory: ManagedDir,
    name: OsString,
    _active: ActiveTemp,
    armed: bool,
}

struct CleanupPlan {
    entries: Vec<CleanupPlanEntry>,
}

enum CleanupPlanEntry {
    File {
        name: OsString,
        kind: EntryKind,
    },
    Directory {
        name: OsString,
        directory: ManagedDir,
        children: CleanupPlan,
    },
}

struct CleanupBudget {
    remaining: usize,
}

impl CleanupBudget {
    fn reserve(&mut self, count: usize) -> Result<(), LoaderError> {
        self.remaining = self.remaining.checked_sub(count).ok_or_else(|| {
            LoaderError::Verify(
                "managed loader cleanup tree exceeds the aggregate entry budget".to_string(),
            )
        })?;
        Ok(())
    }
}

impl ActiveTemp {
    fn register(directory: platform::DirectoryIdentity, name: &str) -> Self {
        let key = ActiveTempKey {
            directory,
            name: OsString::from(name),
        };
        active_temps()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(key.clone());
        Self { key }
    }
}

impl Drop for ActiveTemp {
    fn drop(&mut self) {
        active_temps()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .remove(&self.key);
    }
}

impl PendingTemp {
    fn arm(directory: ManagedDir, name: &str, active: ActiveTemp) -> Self {
        Self {
            _active: active,
            directory,
            name: OsString::from(name),
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for PendingTemp {
    fn drop(&mut self) {
        if self.armed {
            let _ = platform::remove_file(
                &self.directory.inner.handle,
                &self.directory.inner.path,
                &self.name,
            );
        }
    }
}

fn active_temps() -> &'static Mutex<HashSet<ActiveTempKey>> {
    ACTIVE_TEMPS.get_or_init(|| Mutex::new(HashSet::new()))
}

impl ManagedDir {
    pub(crate) fn open_root(path: &Path) -> Result<Self, LoaderError> {
        let (handle, identity) = platform::open_exact_directory(path)?;
        Ok(Self {
            inner: Arc::new(ManagedDirInner {
                path: path.to_path_buf(),
                identity,
                handle,
                binding: DirectoryBinding::Root,
            }),
        })
    }

    pub(crate) fn path(&self) -> &Path {
        &self.inner.path
    }

    pub(crate) fn identity(&self) -> Result<ManagedDirectoryIdentity, LoaderError> {
        self.revalidate()?;
        Ok(ManagedDirectoryIdentity(self.inner.identity))
    }

    pub(crate) fn open_or_create_child(&self, name: &str) -> Result<Self, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        match platform::open_child_directory(&self.inner.handle, &self.inner.path, OsStr::new(name))
        {
            Ok((handle, identity)) => self.child(name, handle, identity),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match platform::create_child_directory(
                    &self.inner.handle,
                    &self.inner.path,
                    OsStr::new(name),
                ) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(LoaderError::Io(error)),
                }
                let (handle, identity) = platform::open_child_directory(
                    &self.inner.handle,
                    &self.inner.path,
                    OsStr::new(name),
                )?;
                self.child(name, handle, identity)
            }
            Err(error) => Err(LoaderError::Io(error)),
        }
    }

    pub(crate) fn open_or_create_persistent_file(
        &self,
        name: &str,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        let name = OsString::from(name);
        let file = match platform::open_file_read_write(&self.inner.handle, &self.inner.path, &name)
        {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match platform::create_new_file(&self.inner.handle, &self.inner.path, &name) {
                    Ok(file) => drop(file),
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(LoaderError::Io(error)),
                }
                platform::open_file_read_write(&self.inner.handle, &self.inner.path, &name)?
            }
            Err(error) => return Err(LoaderError::Io(error)),
        };
        self.bind_persistent_file(name, file)
    }

    pub(crate) fn open_persistent_file(
        &self,
        name: &str,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        let name = OsString::from(name);
        let file = platform::open_file_read_write(&self.inner.handle, &self.inner.path, &name)?;
        self.bind_persistent_file(name, file)
    }

    fn bind_persistent_file(
        &self,
        name: OsString,
        file: std::fs::File,
    ) -> Result<ManagedPersistentFile, LoaderError> {
        let persistent = ManagedPersistentFile {
            directory: self.clone(),
            name,
            identity: platform::file_identity(&file)?,
            file,
        };
        persistent.revalidate()?;
        Ok(persistent)
    }

    pub(crate) fn open_child(&self, name: &str) -> Result<Self, LoaderError> {
        validate_segment(name)?;
        let (handle, identity) =
            platform::open_child_directory(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        self.child(name, handle, identity)
    }

    pub(crate) fn entries_bounded(&self, limit: usize) -> Result<Vec<OsString>, LoaderError> {
        if limit == 0 || limit > MAX_MANAGED_DIRECTORY_ENTRIES + 1 {
            return Err(LoaderError::Verify(
                "managed directory listing bound is invalid".to_string(),
            ));
        }
        self.revalidate()?;
        Ok(platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            limit,
        )?)
    }

    pub(crate) fn has_portably_exact_child_name(
        &self,
        expected: &str,
    ) -> Result<bool, LoaderError> {
        validate_segment(expected)?;
        let expected_folded = expected
            .chars()
            .flat_map(char::to_lowercase)
            .collect::<String>();
        let entries = self.entries_bounded(MAX_MANAGED_DIRECTORY_ENTRIES + 1)?;
        if entries.len() > MAX_MANAGED_DIRECTORY_ENTRIES {
            return Err(LoaderError::Verify(
                "managed directory exceeds the portable alias scan bound".to_string(),
            ));
        }
        let mut matching_name = None;
        for entry in entries {
            let Some(entry) = entry.to_str() else {
                return Err(LoaderError::Verify(
                    "managed directory contains a non-portable entry name".to_string(),
                ));
            };
            let folded = entry
                .chars()
                .flat_map(char::to_lowercase)
                .collect::<String>();
            if folded != expected_folded {
                continue;
            }
            if matching_name.replace(entry.to_string()).is_some() || entry != expected {
                return Err(LoaderError::Verify(
                    "managed directory contains a portable case alias".to_string(),
                ));
            }
        }
        Ok(matching_name.is_some())
    }

    pub(crate) fn inspect_regular_file(
        &self,
        name: &str,
    ) -> Result<Option<ManagedFileGuard>, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        match platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))? {
            None => Ok(None),
            Some(EntryKind::File) => {
                let file = platform::open_file_read(
                    &self.inner.handle,
                    &self.inner.path,
                    OsStr::new(name),
                )?;
                let guard = ManagedFileGuard {
                    identity: platform::file_identity(&file)?,
                    size: file.metadata()?.len(),
                    file,
                };
                if !self.file_guard_matches(name, &guard)? {
                    return Err(LoaderError::Verify(
                        "managed file identity changed during admission".to_string(),
                    ));
                }
                Ok(Some(guard))
            }
            Some(EntryKind::Directory | EntryKind::Link) => Err(LoaderError::Verify(
                "managed file entry has an unsupported type".to_string(),
            )),
            #[cfg(unix)]
            Some(EntryKind::Other) => Err(LoaderError::Verify(
                "managed file entry has an unsupported type".to_string(),
            )),
        }
    }

    pub(crate) fn file_guard_matches(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<bool, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        let current = match platform::open_file_read(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(name),
        ) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => return Err(LoaderError::Io(error)),
        };
        Ok(platform::file_identity(&current)? == guard.identity
            && platform::file_identity(&guard.file)? == guard.identity)
    }

    pub(crate) fn sha1_guarded_file(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<String, LoaderError> {
        validate_segment(name)?;
        if guard.size > max_size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source is invalid or exceeds its bound".to_string(),
            ));
        }
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::file_identity(&file)? != guard.identity || file.metadata()?.len() != guard.size
        {
            return Err(LoaderError::Verify(
                "managed guarded hash source identity changed".to_string(),
            ));
        }
        let mut observed = 0_u64;
        let mut hasher = Sha1::new();
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            observed = observed.checked_add(read as u64).ok_or_else(|| {
                LoaderError::Verify("managed guarded hash size overflowed".to_string())
            })?;
            if observed > guard.size {
                return Err(LoaderError::Verify(
                    "managed guarded hash source exceeded its admitted size".to_string(),
                ));
            }
            hasher.update(&chunk[..read]);
        }
        if observed != guard.size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded hash source changed during hashing".to_string(),
            ));
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    pub(crate) fn read_guarded_file_bounded(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        max_size: u64,
    ) -> Result<Vec<u8>, LoaderError> {
        validate_segment(name)?;
        if guard.size > max_size || !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed guarded read source is invalid or exceeds its bound".to_string(),
            ));
        }
        let mut file = guard.file.try_clone()?;
        if platform::file_identity(&file)? != guard.identity || file.metadata()?.len() != guard.size
        {
            return Err(LoaderError::Verify(
                "managed guarded read source identity changed".to_string(),
            ));
        }
        file.seek(SeekFrom::Start(0))?;
        let capacity = usize::try_from(guard.size)
            .map_err(|_| LoaderError::Verify("managed guarded read size overflowed".to_string()))?;
        let mut bytes = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(max_size.saturating_add(1))
            .read_to_end(&mut bytes)?;
        file.seek(SeekFrom::Start(0))?;
        let mut stable = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(max_size.saturating_add(1))
            .read_to_end(&mut stable)?;
        if bytes.len() as u64 != guard.size
            || stable != bytes
            || !self.file_guard_matches(name, guard)?
        {
            return Err(LoaderError::Verify(
                "managed guarded read source changed during reading".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn rename_guarded_file_no_replace(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
        destination: &ManagedDir,
        destination_name: &str,
    ) -> Result<(), LoaderError> {
        validate_segment(name)?;
        validate_segment(destination_name)?;
        if !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed rename source identity changed".to_string(),
            ));
        }
        destination.revalidate()?;
        platform::rename_entry_no_replace(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(name),
            &destination.inner.handle,
            &destination.inner.path,
            OsStr::new(destination_name),
        )?;
        if !destination.file_guard_matches(destination_name, guard)? {
            return Err(LoaderError::Verify(
                "managed rename destination identity changed".to_string(),
            ));
        }
        self.revalidate()?;
        Ok(())
    }

    pub(crate) fn sync(&self) -> Result<(), LoaderError> {
        self.revalidate()?;
        platform::sync_directory(&self.inner.handle)?;
        self.revalidate()
    }

    fn child(
        &self,
        name: &str,
        handle: platform::DirectoryHandle,
        identity: platform::DirectoryIdentity,
    ) -> Result<Self, LoaderError> {
        let child = Self {
            inner: Arc::new(ManagedDirInner {
                path: self.inner.path.join(name),
                identity,
                handle,
                binding: DirectoryBinding::Child {
                    parent: self.inner.clone(),
                    name: OsString::from(name),
                },
            }),
        };
        child.revalidate()?;
        Ok(child)
    }

    pub(crate) fn open_or_create_relative_parent(
        &self,
        relative: &ArtifactRelativePath,
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
            "managed artifact path has no filename".to_string(),
        ))
    }

    pub(crate) async fn write_relative_exact(
        &self,
        relative: &ArtifactRelativePath,
        bytes: &[u8],
    ) -> Result<(), LoaderError> {
        let (parent, name) = self.open_or_create_relative_parent(relative)?;
        parent.write_exact(&name, bytes).await
    }

    pub(crate) async fn import_relative_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        source: std::fs::File,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<(), LoaderError> {
        if expected_size == 0 || expected_size > MAX_MANAGED_TREE_FILE_BYTES {
            return Err(LoaderError::Verify(
                "managed loader source exceeds the processor stage file bound".to_string(),
            ));
        }
        let (parent, name) = self.open_or_create_relative_parent(relative)?;
        tokio::task::spawn_blocking(move || {
            parent.import_authenticated(&name, source, expected_size, expected_sha1)
        })
        .await
        .map_err(|_| {
            LoaderError::Verify(
                "managed loader source import task stopped unexpectedly".to_string(),
            )
        })?
    }

    fn import_authenticated(
        &self,
        name: &str,
        mut source: std::fs::File,
        expected_size: u64,
        expected_sha1: [u8; 20],
    ) -> Result<(), LoaderError> {
        validate_segment(name)?;
        source.seek(SeekFrom::Start(0))?;
        let temp_name = temp_name();
        self.sweep_orphan_temps()?;
        let active = ActiveTemp::register(self.inner.identity, &temp_name);
        let mut destination = platform::create_new_file(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
        )?;
        let mut pending = PendingTemp::arm(self.clone(), &temp_name, active);
        let mut observed = 0_u64;
        let mut hasher = Sha1::new();
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = source.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            observed = observed.checked_add(read as u64).ok_or_else(|| {
                LoaderError::Verify("managed loader source size overflowed".to_string())
            })?;
            if observed > expected_size {
                return Err(LoaderError::Verify(
                    "managed loader source exceeds its authenticated size".to_string(),
                ));
            }
            destination.write_all(&chunk[..read])?;
            hasher.update(&chunk[..read]);
        }
        if observed != expected_size || <[u8; 20]>::from(hasher.finalize()) != expected_sha1 {
            return Err(LoaderError::Verify(
                "managed loader source failed authenticated integrity".to_string(),
            ));
        }
        destination.flush()?;
        destination.sync_all()?;
        drop(destination);
        if platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))?.is_some() {
            platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        }
        platform::rename_entry(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(name),
        )?;
        pending.disarm();
        let mut budget = TreeCaptureBudget {
            remaining_entries: 1,
            remaining_bytes: expected_size,
        };
        let fact = self.capture_file_fact(
            name,
            ManagedTreeLimits {
                max_entries: 1,
                max_depth: 0,
                max_file_bytes: expected_size,
                max_total_bytes: expected_size,
            },
            &mut budget,
        )?;
        if fact.size != expected_size || fact.sha1 != expected_sha1 {
            let _ = platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name));
            return Err(LoaderError::Verify(
                "managed loader import changed before promotion".to_string(),
            ));
        }
        self.revalidate()
    }

    pub(crate) async fn materialize_installer_library(
        &self,
        source: RetainedInstallerLibrarySource,
    ) -> Result<MaterializedInstallerLibrary, LoaderError> {
        let destination = source.path().join_under(&self.inner.path);
        self.write_relative_exact(source.path(), source.bytes())
            .await?;
        Ok(MaterializedInstallerLibrary {
            source,
            destination,
        })
    }

    pub(crate) async fn write_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        validate_segment(name)?;
        let temp_name = temp_name();
        self.sweep_orphan_temps()?;
        let active = ActiveTemp::register(self.inner.identity, &temp_name);
        let file = platform::create_new_file(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
        )?;
        let mut pending = PendingTemp::arm(self.clone(), &temp_name, active);
        let mut file = tokio::fs::File::from_std(file);
        let write_result = async {
            file.write_all(bytes).await?;
            file.flush().await?;
            file.sync_all().await?;
            file.seek(SeekFrom::Start(0)).await?;
            let mut written = Vec::with_capacity(bytes.len());
            file.read_to_end(&mut written).await?;
            if written != bytes {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "managed loader temp bytes changed before promotion",
                ));
            }
            Ok::<(), io::Error>(())
        }
        .await;
        drop(file);
        if let Err(error) = write_result {
            return Err(LoaderError::Io(error));
        }
        if platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))?.is_some() {
            platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        }
        if let Err(error) = platform::rename_entry(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(name),
        ) {
            return Err(LoaderError::Io(error));
        }
        pending.disarm();
        if self.read_bounded(name, bytes.len() as u64, true)? != bytes {
            let _ = platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name));
            return Err(LoaderError::Verify(
                "installed loader artifact differs from authenticated bytes".to_string(),
            ));
        }
        self.revalidate()
    }

    pub(crate) fn write_new_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        validate_segment(name)?;
        let temp_name = temp_name();
        self.sweep_orphan_temps()?;
        let active = ActiveTemp::register(self.inner.identity, &temp_name);
        let mut file = platform::create_new_file(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
        )?;
        let mut pending = PendingTemp::arm(self.clone(), &temp_name, active);
        file.write_all(bytes)?;
        file.flush()?;
        file.sync_all()?;
        file.seek(SeekFrom::Start(0))?;
        let write_result = verify_reader_exact_bytes(&mut file, bytes);
        drop(file);
        match write_result {
            Ok(true) => {}
            Ok(false) => {
                return Err(LoaderError::Verify(
                    "managed transaction temp bytes changed before promotion".to_string(),
                ));
            }
            Err(error) => return Err(LoaderError::Io(error)),
        }
        platform::rename_entry_no_replace(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(name),
        )?;
        pending.disarm();
        self.verify_exact_bytes(name, bytes)?;
        self.revalidate()
    }

    pub(crate) fn verify_authenticated(
        &self,
        name: &str,
        expected_size: u64,
        expected_sha1: &str,
    ) -> Result<(), LoaderError> {
        if expected_size > MAX_MANAGED_READ_BYTES {
            return Err(LoaderError::Verify(
                "managed authenticated verification exceeds the admitted size bound".to_string(),
            ));
        }
        validate_segment(name)?;
        self.revalidate()?;
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        let identity = platform::file_identity(&file)?;
        if file.metadata()?.len() != expected_size {
            return Err(LoaderError::Verify(
                "managed authenticated file size changed".to_string(),
            ));
        }
        let mut observed = 0_u64;
        let mut hasher = Sha1::new();
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            observed = observed.checked_add(read as u64).ok_or_else(|| {
                LoaderError::Verify("managed authenticated file size overflowed".to_string())
            })?;
            if observed > expected_size {
                return Err(LoaderError::Verify(
                    "managed authenticated file exceeds its admitted size".to_string(),
                ));
            }
            hasher.update(&chunk[..read]);
        }
        let digest = format!("{:x}", hasher.finalize());
        if observed != expected_size || !digest.eq_ignore_ascii_case(expected_sha1) {
            return Err(LoaderError::Verify(
                "managed authenticated file failed integrity verification".to_string(),
            ));
        }
        let current =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::file_identity(&current)? != identity
            || platform::file_identity(&file)? != identity
        {
            return Err(LoaderError::Verify(
                "managed authenticated file identity changed".to_string(),
            ));
        }
        self.revalidate()
    }

    fn verify_exact_bytes(&self, name: &str, expected: &[u8]) -> Result<(), LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        let identity = platform::file_identity(&file)?;
        let expected_size = u64::try_from(expected.len()).map_err(|_| {
            LoaderError::Verify("managed transaction artifact size overflowed".to_string())
        })?;
        if file.metadata()?.len() != expected_size
            || !verify_reader_exact_bytes(&mut file, expected)?
        {
            return Err(LoaderError::Verify(
                "managed transaction artifact changed after promotion".to_string(),
            ));
        }
        let current =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::file_identity(&current)? != identity
            || platform::file_identity(&file)? != identity
        {
            return Err(LoaderError::Verify(
                "managed transaction artifact identity changed".to_string(),
            ));
        }
        self.revalidate()
    }

    pub(crate) fn read_authenticated(
        &self,
        name: &str,
        expected_size: Option<u64>,
        expected_sha1: Option<&str>,
    ) -> Result<Vec<u8>, LoaderError> {
        if expected_size.is_some_and(|size| size > MAX_MANAGED_READ_BYTES) {
            return Err(LoaderError::Verify(
                "managed loader source exceeds the admitted size bound".to_string(),
            ));
        }
        let limit = expected_size.unwrap_or(MAX_MANAGED_READ_BYTES);
        let bytes = self.read_bounded(name, limit, expected_size.is_some())?;
        if expected_size.is_some_and(|size| size != bytes.len() as u64)
            || expected_sha1.is_some_and(|sha1| {
                !sha1.eq_ignore_ascii_case(&format!("{:x}", Sha1::digest(&bytes)))
            })
        {
            return Err(LoaderError::Verify(
                "managed loader source bytes failed authenticated integrity".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn read_relative_authenticated(
        &self,
        relative: &ArtifactRelativePath,
        expected_size: Option<u64>,
        expected_sha1: &[u8; 20],
    ) -> Result<Vec<u8>, LoaderError> {
        if expected_size.is_some_and(|size| size > MAX_MANAGED_READ_BYTES) {
            return Err(LoaderError::Verify(
                "managed relative read exceeds the admitted size bound".to_string(),
            ));
        }
        self.revalidate()?;
        let mut segments = relative.as_str().split('/').peekable();
        let mut directory = self.clone();
        while let Some(segment) = segments.next() {
            if segments.peek().is_none() {
                let bytes = directory.read_bounded(
                    segment,
                    expected_size.unwrap_or(MAX_MANAGED_READ_BYTES),
                    expected_size.is_some(),
                )?;
                let actual_sha1: [u8; 20] = Sha1::digest(&bytes).into();
                if &actual_sha1 != expected_sha1 {
                    return Err(LoaderError::Verify(
                        "managed relative read failed authenticated integrity".to_string(),
                    ));
                }
                directory.revalidate()?;
                self.revalidate()?;
                return Ok(bytes);
            }
            directory = directory.open_child(segment)?;
        }
        Err(LoaderError::Verify(
            "managed relative read has no final file".to_string(),
        ))
    }

    pub(crate) fn snapshot_tree(
        &self,
        limits: ManagedTreeLimits,
    ) -> Result<ManagedTreeSnapshot, LoaderError> {
        self.snapshot_tree_with(limits, || Ok(()))
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

    pub(crate) fn validate_exact_child_directories(
        &self,
        expected: &[&str],
    ) -> Result<(), LoaderError> {
        self.revalidate()?;
        let names = platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            expected.len().saturating_add(1),
        )?;
        if names.len() != expected.len() {
            return Err(LoaderError::Verify(
                "managed root contains unexpected entries".to_string(),
            ));
        }
        let expected = expected.iter().copied().collect::<BTreeSet<_>>();
        for name in names {
            let name = name.to_str().ok_or_else(|| {
                LoaderError::Verify("managed root contains a non-UTF-8 entry".to_string())
            })?;
            if !expected.contains(name)
                || !matches!(
                    platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))?,
                    Some(EntryKind::Directory)
                )
            {
                return Err(LoaderError::Verify(
                    "managed root child identity is invalid".to_string(),
                ));
            }
        }
        self.revalidate()
    }

    fn validate_tree_usage(
        &self,
        limits: ManagedTreeLimits,
        allow_links: bool,
    ) -> Result<ManagedTreeUsage, LoaderError> {
        self.revalidate()?;
        let mut aliases = HashMap::new();
        let mut budget = TreeCaptureBudget {
            remaining_entries: limits.max_entries,
            remaining_bytes: limits.max_total_bytes,
        };
        self.validate_tree_directory(None, 0, limits, &mut budget, &mut aliases, allow_links)?;
        self.revalidate()?;
        Ok(ManagedTreeUsage {
            entries: limits.max_entries - budget.remaining_entries,
            bytes: limits.max_total_bytes - budget.remaining_bytes,
        })
    }

    fn validate_tree_directory(
        &self,
        prefix: Option<&str>,
        depth: usize,
        limits: ManagedTreeLimits,
        budget: &mut TreeCaptureBudget,
        aliases: &mut HashMap<String, String>,
        allow_links: bool,
    ) -> Result<(), LoaderError> {
        self.revalidate()?;
        let entries = platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            budget.remaining_entries.saturating_add(1),
        )?;
        if entries.len() > budget.remaining_entries {
            return Err(LoaderError::Verify(
                "managed tree exceeds the aggregate entry bound".to_string(),
            ));
        }
        budget.remaining_entries -= entries.len();
        for name in entries {
            let name = name.to_str().ok_or_else(|| {
                LoaderError::Verify("managed tree contains a non-UTF-8 entry".to_string())
            })?;
            validate_segment(name)?;
            let authored = match prefix {
                Some(prefix) => format!("{prefix}/{name}"),
                None => name.to_string(),
            };
            let relative = ArtifactRelativePath::new(&authored).map_err(|_| {
                LoaderError::Verify("managed tree path is not canonical".to_string())
            })?;
            insert_tree_alias(aliases, &relative)?;
            match platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))? {
                Some(EntryKind::File) => self.validate_file_size(name, limits, budget)?,
                Some(EntryKind::Directory) => {
                    if depth >= limits.max_depth {
                        return Err(LoaderError::Verify(
                            "managed tree exceeds the depth bound".to_string(),
                        ));
                    }
                    self.open_child(name)?.validate_tree_directory(
                        Some(&authored),
                        depth + 1,
                        limits,
                        budget,
                        aliases,
                        allow_links,
                    )?;
                }
                Some(EntryKind::Link) if allow_links => {}
                Some(EntryKind::Link) | None => {
                    return Err(LoaderError::Verify(
                        "managed tree contains a link or replaced entry".to_string(),
                    ));
                }
                #[cfg(unix)]
                Some(EntryKind::Other) => {
                    return Err(LoaderError::Verify(
                        "managed tree contains an unsupported entry".to_string(),
                    ));
                }
            }
        }
        self.revalidate()
    }

    fn validate_file_size(
        &self,
        name: &str,
        limits: ManagedTreeLimits,
        budget: &mut TreeCaptureBudget,
    ) -> Result<(), LoaderError> {
        let file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        let size = file.metadata()?.len().max(file.metadata()?.len());
        if size > limits.max_file_bytes || size > budget.remaining_bytes {
            return Err(LoaderError::Verify(
                "managed tree file exceeds its admitted byte bound".to_string(),
            ));
        }
        budget.remaining_bytes -= size;
        Ok(())
    }

    fn snapshot_tree_with(
        &self,
        limits: ManagedTreeLimits,
        between_captures: impl FnOnce() -> Result<(), LoaderError>,
    ) -> Result<ManagedTreeSnapshot, LoaderError> {
        self.revalidate()?;
        let first = self.capture_tree_once(limits)?;
        between_captures()?;
        self.revalidate()?;
        let second = self.capture_tree_once(limits)?;
        self.revalidate()?;
        if first != second {
            return Err(LoaderError::Verify(
                "managed tree changed during bounded snapshot".to_string(),
            ));
        }
        Ok(second)
    }

    fn capture_tree_once(
        &self,
        limits: ManagedTreeLimits,
    ) -> Result<ManagedTreeSnapshot, LoaderError> {
        let mut snapshot = ManagedTreeSnapshot::default();
        let mut aliases = HashMap::new();
        let mut budget = TreeCaptureBudget {
            remaining_entries: limits.max_entries,
            remaining_bytes: limits.max_total_bytes,
        };
        self.capture_tree_directory(None, 0, limits, &mut budget, &mut aliases, &mut snapshot)?;
        Ok(snapshot)
    }

    fn capture_tree_directory(
        &self,
        prefix: Option<&str>,
        depth: usize,
        limits: ManagedTreeLimits,
        budget: &mut TreeCaptureBudget,
        aliases: &mut HashMap<String, String>,
        snapshot: &mut ManagedTreeSnapshot,
    ) -> Result<(), LoaderError> {
        self.revalidate()?;
        let entries = platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            budget.remaining_entries.saturating_add(1),
        )?;
        if entries.len() > budget.remaining_entries {
            return Err(LoaderError::Verify(
                "managed tree exceeds the aggregate entry bound".to_string(),
            ));
        }
        budget.remaining_entries -= entries.len();
        let mut names = entries;
        names.sort();
        for name in names {
            let name = name.to_str().ok_or_else(|| {
                LoaderError::Verify("managed tree contains a non-UTF-8 entry".to_string())
            })?;
            validate_segment(name)?;
            let authored = match prefix {
                Some(prefix) => format!("{prefix}/{name}"),
                None => name.to_string(),
            };
            let relative = ArtifactRelativePath::new(&authored).map_err(|_| {
                LoaderError::Verify("managed tree path is not canonical".to_string())
            })?;
            insert_tree_alias(aliases, &relative)?;
            match platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))? {
                Some(EntryKind::File) => {
                    let fact = self.capture_file_fact(name, limits, budget)?;
                    snapshot.files.insert(relative, fact);
                }
                Some(EntryKind::Directory) => {
                    if depth >= limits.max_depth {
                        return Err(LoaderError::Verify(
                            "managed tree exceeds the depth bound".to_string(),
                        ));
                    }
                    let child = self.open_child(name)?;
                    snapshot.directories.insert(relative);
                    child.capture_tree_directory(
                        Some(&authored),
                        depth + 1,
                        limits,
                        budget,
                        aliases,
                        snapshot,
                    )?;
                }
                Some(EntryKind::Link) | None => {
                    return Err(LoaderError::Verify(
                        "managed tree contains a link or replaced entry".to_string(),
                    ));
                }
                #[cfg(unix)]
                Some(EntryKind::Other) => {
                    return Err(LoaderError::Verify(
                        "managed tree contains an unsupported entry".to_string(),
                    ));
                }
            }
        }
        self.revalidate()
    }

    fn capture_file_fact(
        &self,
        name: &str,
        limits: ManagedTreeLimits,
        budget: &mut TreeCaptureBudget,
    ) -> Result<ManagedFileFact, LoaderError> {
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        let identity = platform::file_identity(&file)?;
        let metadata = file.metadata()?;
        let size = metadata.len();
        if size > limits.max_file_bytes || size > budget.remaining_bytes {
            return Err(LoaderError::Verify(
                "managed tree file exceeds its admitted byte bound".to_string(),
            ));
        }
        let mut hasher = Sha1::new();
        let mut observed = 0_u64;
        let mut chunk = [0_u8; 64 * 1024];
        loop {
            let read = file.read(&mut chunk)?;
            if read == 0 {
                break;
            }
            observed = observed.checked_add(read as u64).ok_or_else(|| {
                LoaderError::Verify("managed tree byte count overflowed".to_string())
            })?;
            if observed > size {
                return Err(LoaderError::Verify(
                    "managed tree file changed during snapshot".to_string(),
                ));
            }
            hasher.update(&chunk[..read]);
        }
        if observed != size || file.metadata()?.len() != size {
            return Err(LoaderError::Verify(
                "managed tree file changed during snapshot".to_string(),
            ));
        }
        let current =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::file_identity(&current)? != identity || current.metadata()?.len() != size {
            return Err(LoaderError::Verify(
                "managed tree file identity changed during snapshot".to_string(),
            ));
        }
        budget.remaining_bytes -= size;
        Ok(ManagedFileFact {
            size,
            sha1: hasher.finalize().into(),
        })
    }

    fn read_bounded(
        &self,
        name: &str,
        limit: u64,
        require_exact_len: bool,
    ) -> Result<Vec<u8>, LoaderError> {
        validate_segment(name)?;
        self.revalidate()?;
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        let identity = platform::file_identity(&file)?;
        let metadata = file.metadata()?;
        if metadata.len() > limit || (require_exact_len && metadata.len() != limit) {
            return Err(LoaderError::Verify(
                "managed loader artifact exceeds its admitted size".to_string(),
            ));
        }
        let capacity = usize::try_from(metadata.len()).map_err(|_| {
            LoaderError::Verify("managed loader artifact size is out of range".to_string())
        })?;
        let mut bytes = Vec::with_capacity(capacity);
        Read::by_ref(&mut file)
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit || (require_exact_len && bytes.len() as u64 != limit) {
            return Err(LoaderError::Verify(
                "managed loader artifact changed during bounded read".to_string(),
            ));
        }
        if file.metadata()?.len() != metadata.len() {
            return Err(LoaderError::Verify(
                "managed loader artifact changed during bounded read".to_string(),
            ));
        }
        let current =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::file_identity(&current)? != identity
            || current.metadata()?.len() != metadata.len()
        {
            return Err(LoaderError::Verify(
                "managed loader artifact identity changed during bounded read".to_string(),
            ));
        }
        self.revalidate()?;
        Ok(bytes)
    }

    pub(crate) fn remove_guarded_file(
        &self,
        name: &str,
        guard: &ManagedFileGuard,
    ) -> Result<(), LoaderError> {
        validate_segment(name)?;
        if !self.file_guard_matches(name, guard)? {
            return Err(LoaderError::Verify(
                "managed cleanup source identity changed".to_string(),
            ));
        }
        platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        if platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))?.is_some() {
            return Err(LoaderError::Verify(
                "managed cleanup source remained after removal".to_string(),
            ));
        }
        self.revalidate()
    }

    pub(crate) fn clear_owned_contents(self) -> Result<(), LoaderError> {
        if !matches!(&self.inner.binding, DirectoryBinding::Child { .. }) {
            return Err(LoaderError::Verify(
                "managed root cannot be recursively cleared".to_string(),
            ));
        }
        self.clear_contents()
    }

    fn clear_contents(&self) -> Result<(), LoaderError> {
        self.clear_contents_bounded(MAX_MANAGED_DIRECTORY_ENTRIES)
    }

    fn clear_contents_bounded(&self, entry_limit: usize) -> Result<(), LoaderError> {
        let mut budget = CleanupBudget {
            remaining: entry_limit,
        };
        let plan = self.plan_cleanup(0, &mut budget)?;
        self.execute_cleanup(&plan)?;
        self.validate_cleanup_result(&plan)
    }

    fn plan_cleanup(
        &self,
        depth: usize,
        budget: &mut CleanupBudget,
    ) -> Result<CleanupPlan, LoaderError> {
        if depth > 16 {
            return Err(LoaderError::Verify(
                "managed loader cleanup tree is too deep".to_string(),
            ));
        }
        self.revalidate()?;
        let scan_limit = budget.remaining.saturating_add(1);
        let entries = platform::entry_names(&self.inner.handle, &self.inner.path, scan_limit)?;
        budget.reserve(entries.len())?;
        let mut planned = Vec::with_capacity(entries.len());
        for name in entries {
            let Some(name_text) = name.to_str() else {
                return Err(LoaderError::Verify(
                    "managed loader cleanup contains a non-UTF-8 entry".to_string(),
                ));
            };
            validate_segment(name_text)?;
            match platform::entry_kind(&self.inner.handle, &self.inner.path, &name)? {
                None => {}
                Some(EntryKind::Directory) => {
                    let child = self.open_child(name_text)?;
                    let children = child.plan_cleanup(depth + 1, budget)?;
                    planned.push(CleanupPlanEntry::Directory {
                        name,
                        directory: child,
                        children,
                    });
                }
                Some(kind @ (EntryKind::File | EntryKind::Link)) => {
                    planned.push(CleanupPlanEntry::File { name, kind });
                }
                #[cfg(unix)]
                Some(EntryKind::Other) => {
                    return Err(LoaderError::Verify(
                        "managed loader cleanup contains an unsupported entry".to_string(),
                    ));
                }
            }
        }
        self.revalidate()?;
        Ok(CleanupPlan { entries: planned })
    }

    fn execute_cleanup(&self, plan: &CleanupPlan) -> Result<(), LoaderError> {
        for entry in &plan.entries {
            match entry {
                CleanupPlanEntry::File { name, kind } => {
                    match platform::entry_kind(&self.inner.handle, &self.inner.path, name)? {
                        None => {}
                        Some(actual) if actual == *kind => {
                            platform::remove_file(&self.inner.handle, &self.inner.path, name)?;
                        }
                        Some(_) => {
                            return Err(LoaderError::Verify(
                                "managed loader cleanup entry changed after preflight".to_string(),
                            ));
                        }
                    }
                }
                CleanupPlanEntry::Directory {
                    name: _,
                    directory,
                    children,
                } => {
                    directory.execute_cleanup(children)?;
                }
            }
        }
        self.revalidate()
    }

    fn validate_cleanup_result(&self, plan: &CleanupPlan) -> Result<(), LoaderError> {
        self.revalidate()?;
        let mut expected_directories = plan
            .entries
            .iter()
            .filter_map(|entry| match entry {
                CleanupPlanEntry::Directory { name, .. } => Some(name.clone()),
                CleanupPlanEntry::File { .. } => None,
            })
            .collect::<HashSet<_>>();
        let entries = platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            expected_directories.len().saturating_add(1),
        )?;
        if entries.len() != expected_directories.len()
            || entries
                .iter()
                .any(|name| !expected_directories.remove(name))
            || !expected_directories.is_empty()
        {
            return Err(LoaderError::Verify(
                "managed loader cleanup result contains unplanned entries".to_string(),
            ));
        }
        for entry in &plan.entries {
            if let CleanupPlanEntry::Directory {
                directory,
                children,
                ..
            } = entry
            {
                directory.revalidate()?;
                directory.validate_cleanup_result(children)?;
            }
        }
        self.revalidate()
    }

    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        let actual = match &self.inner.binding {
            DirectoryBinding::Root => platform::directory_identity_at_path(&self.inner.path)?,
            DirectoryBinding::Child { parent, name } => {
                platform::child_directory_identity(&parent.handle, &parent.path, name)?
            }
        };
        if actual != self.inner.identity {
            return Err(LoaderError::Verify(
                "managed loader directory identity changed during mutation".to_string(),
            ));
        }
        if let DirectoryBinding::Child { parent, .. } = &self.inner.binding {
            ManagedDir {
                inner: parent.clone(),
            }
            .revalidate()?;
        }
        Ok(())
    }

    pub(crate) fn sweep_orphan_temps(&self) -> Result<(), LoaderError> {
        let mut system = System::new();
        self.sweep_orphan_temps_with(|pid| temp_owner_is_live(&mut system, pid))
    }

    fn sweep_orphan_temps_with<F>(&self, mut owner_is_live: F) -> Result<(), LoaderError>
    where
        F: FnMut(u32) -> bool,
    {
        let entries = platform::entry_names(
            &self.inner.handle,
            &self.inner.path,
            MAX_MANAGED_DIRECTORY_ENTRIES + 1,
        )?;
        if entries.len() > MAX_MANAGED_DIRECTORY_ENTRIES {
            return Err(LoaderError::Verify(
                "managed loader directory exceeds the bounded entry scan".to_string(),
            ));
        }
        let mut reserved = Vec::new();
        for name in entries {
            let Some(text) = name.to_str() else { continue };
            if !text.starts_with(TEMP_PREFIX) {
                continue;
            }
            let owner_pid = temp_owner_pid(text).ok_or_else(|| {
                LoaderError::Verify(
                    "managed loader temp namespace contains a malformed entry".to_string(),
                )
            })?;
            reserved.push((name, owner_pid));
        }
        if reserved.len() > MAX_TEMP_SWEEP_ENTRIES {
            return Err(LoaderError::Verify(
                "managed loader directory exceeds the bounded temp sweep".to_string(),
            ));
        }
        for (name, owner_pid) in reserved {
            let key = ActiveTempKey {
                directory: self.inner.identity,
                name: name.clone(),
            };
            if active_temps()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .contains(&key)
            {
                continue;
            }
            if owner_pid != std::process::id() && owner_is_live(owner_pid) {
                continue;
            }
            match platform::entry_kind(&self.inner.handle, &self.inner.path, &name)? {
                Some(EntryKind::File | EntryKind::Link) => {
                    platform::remove_file(&self.inner.handle, &self.inner.path, &name)?;
                }
                Some(EntryKind::Directory) => {
                    return Err(LoaderError::Verify(
                        "managed loader temp namespace contains an unsafe entry".to_string(),
                    ));
                }
                #[cfg(unix)]
                Some(EntryKind::Other) => {
                    return Err(LoaderError::Verify(
                        "managed loader temp namespace contains an unsafe entry".to_string(),
                    ));
                }
                None => {}
            }
        }
        Ok(())
    }
}

impl ManagedPersistentFile {
    pub(crate) fn revalidate(&self) -> Result<(), LoaderError> {
        self.directory.revalidate()?;
        let current = platform::open_file_read_write(
            &self.directory.inner.handle,
            &self.directory.inner.path,
            &self.name,
        )?;
        if platform::file_identity(&current)? != self.identity {
            return Err(LoaderError::Verify(
                "managed persistent file identity changed".to_string(),
            ));
        }
        self.directory.revalidate()
    }

    pub(crate) fn try_lock_exclusive(&self) -> Result<bool, LoaderError> {
        self.revalidate()?;
        match self.file.try_lock() {
            Ok(()) => {
                if let Err(error) = self.revalidate() {
                    let _ = self.file.unlock();
                    return Err(error);
                }
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => Err(LoaderError::Io(error)),
        }
    }

    pub(crate) fn try_lock_shared(&self) -> Result<bool, LoaderError> {
        self.revalidate()?;
        match self.file.try_lock_shared() {
            Ok(()) => {
                if let Err(error) = self.revalidate() {
                    let _ = self.file.unlock();
                    return Err(error);
                }
                Ok(true)
            }
            Err(std::fs::TryLockError::WouldBlock) => Ok(false),
            Err(std::fs::TryLockError::Error(error)) => Err(LoaderError::Io(error)),
        }
    }

    pub(crate) fn unlock(&self) -> io::Result<()> {
        self.file.unlock()
    }
}

fn verify_reader_exact_bytes(reader: &mut std::fs::File, expected: &[u8]) -> io::Result<bool> {
    let mut offset = 0_usize;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            return Ok(offset == expected.len());
        }
        let Some(end) = offset.checked_add(read) else {
            return Ok(false);
        };
        if end > expected.len() || chunk[..read] != expected[offset..end] {
            return Ok(false);
        }
        offset = end;
    }
}

fn validate_segment(name: &str) -> Result<(), LoaderError> {
    validate_artifact_path_segment(name).map_err(|_| {
        LoaderError::Verify("managed loader path segment is not canonical".to_string())
    })
}

fn insert_tree_alias(
    aliases: &mut HashMap<String, String>,
    path: &ArtifactRelativePath,
) -> Result<(), LoaderError> {
    let portable = path
        .as_str()
        .chars()
        .flat_map(char::to_lowercase)
        .collect::<String>();
    match aliases.get(&portable) {
        Some(existing) if existing != path.as_str() => Err(LoaderError::Verify(
            "managed tree contains a portable case-fold alias".to_string(),
        )),
        Some(_) => Ok(()),
        None => {
            aliases.insert(portable, path.as_str().to_string());
            Ok(())
        }
    }
}

fn temp_name() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let sequence = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    format!("{TEMP_PREFIX}{}-{nanos:x}-{sequence:x}", std::process::id())
}

fn temp_owner_pid(name: &str) -> Option<u32> {
    let mut parts = name.strip_prefix(TEMP_PREFIX)?.split('-');
    let pid_text = parts.next()?;
    let nanos_text = parts.next()?;
    let sequence_text = parts.next()?;
    if parts.next().is_some() {
        return None;
    }
    let pid = pid_text.parse::<u32>().ok()?;
    let nanos = u128::from_str_radix(nanos_text, 16).ok()?;
    let sequence = u64::from_str_radix(sequence_text, 16).ok()?;
    (pid.to_string() == pid_text
        && format!("{nanos:x}") == nanos_text
        && format!("{sequence:x}") == sequence_text)
        .then_some(pid)
}

fn temp_owner_is_live(system: &mut System, pid: u32) -> bool {
    let pid = Pid::from_u32(pid);
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(&[pid]),
        true,
        ProcessRefreshKind::nothing().without_tasks(),
    );
    system.process(pid).is_some()
}

#[cfg(unix)]
mod platform {
    use super::EntryKind;
    use rustix::fs::{self as rfs, AtFlags, Dir, FileType, Mode, OFlags};
    use std::ffi::{CStr, OsStr, OsString};
    use std::fs;
    use std::io;
    use std::os::fd::OwnedFd;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;

    pub(super) type DirectoryHandle = OwnedFd;
    pub(super) type FileIdentity = DirectoryIdentity;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(super) struct DirectoryIdentity {
        device: u64,
        inode: u64,
    }

    pub(super) fn directory_identity_binding(identity: DirectoryIdentity) -> String {
        format!("unix:{:016x}:{:016x}", identity.device, identity.inode)
    }

    fn directory_flags() -> OFlags {
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
    }

    pub(super) fn open_exact_directory(
        path: &Path,
    ) -> io::Result<(DirectoryHandle, DirectoryIdentity)> {
        let handle = rfs::open(path, directory_flags(), Mode::empty())?;
        let identity = identity_from_stat(rfs::fstat(&handle)?);
        Ok((handle, identity))
    }

    pub(super) fn open_child_directory(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<(DirectoryHandle, DirectoryIdentity)> {
        let handle = rfs::openat(parent, name, directory_flags(), Mode::empty())?;
        let identity = identity_from_stat(rfs::fstat(&handle)?);
        Ok((handle, identity))
    }

    pub(super) fn create_child_directory(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        Ok(rfs::mkdirat(parent, name, Mode::from_bits_truncate(0o700))?)
    }

    pub(super) fn create_new_file(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let fd = rfs::openat(
            parent,
            name,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_bits_truncate(0o600),
        )?;
        Ok(fs::File::from(fd))
    }

    pub(super) fn open_file_read(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let fd = rfs::openat(
            parent,
            name,
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = rfs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not a file",
            ));
        }
        Ok(fs::File::from(fd))
    }

    pub(super) fn open_file_read_write(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let fd = rfs::openat(
            parent,
            name,
            OFlags::RDWR | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )?;
        let stat = rfs::fstat(&fd)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::RegularFile {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not a file",
            ));
        }
        Ok(fs::File::from(fd))
    }

    pub(super) fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
        Ok(identity_from_stat(rfs::fstat(file)?))
    }

    pub(super) fn rename_entry(
        from_parent: &DirectoryHandle,
        _from_path: &Path,
        from: &OsStr,
        to_parent: &DirectoryHandle,
        _to_path: &Path,
        to: &OsStr,
    ) -> io::Result<()> {
        Ok(rfs::renameat(from_parent, from, to_parent, to)?)
    }

    pub(super) fn rename_entry_no_replace(
        from_parent: &DirectoryHandle,
        _from_path: &Path,
        from: &OsStr,
        to_parent: &DirectoryHandle,
        _to_path: &Path,
        to: &OsStr,
    ) -> io::Result<()> {
        #[cfg(any(
            target_os = "android",
            target_os = "ios",
            target_os = "linux",
            target_os = "macos",
            target_os = "redox",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        ))]
        {
            Ok(rfs::renameat_with(
                from_parent,
                from,
                to_parent,
                to,
                rfs::RenameFlags::NOREPLACE,
            )?)
        }
        #[cfg(not(any(
            target_os = "android",
            target_os = "ios",
            target_os = "linux",
            target_os = "macos",
            target_os = "redox",
            target_os = "tvos",
            target_os = "visionos",
            target_os = "watchos"
        )))]
        {
            rfs::linkat(from_parent, from, to_parent, to, AtFlags::empty())?;
            if let Err(error) = rfs::unlinkat(from_parent, from, AtFlags::empty()) {
                let _ = rfs::unlinkat(to_parent, to, AtFlags::empty());
                return Err(error.into());
            }
            Ok(())
        }
    }

    pub(super) fn remove_file(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        Ok(rfs::unlinkat(parent, name, AtFlags::empty())?)
    }

    pub(super) fn sync_directory(directory: &DirectoryHandle) -> io::Result<()> {
        Ok(rfs::fsync(directory)?)
    }

    pub(super) fn entry_kind(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<Option<EntryKind>> {
        match rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW) {
            Ok(stat) => Ok(Some(match FileType::from_raw_mode(stat.st_mode) {
                FileType::RegularFile => EntryKind::File,
                FileType::Directory => EntryKind::Directory,
                FileType::Symlink => EntryKind::Link,
                _ => EntryKind::Other,
            })),
            Err(error) if error == rustix::io::Errno::NOENT => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub(super) fn entry_names(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        limit: usize,
    ) -> io::Result<Vec<OsString>> {
        let mut entries = Dir::read_from(parent)?;
        let mut names = Vec::new();
        while names.len() < limit {
            let Some(entry) = entries.next() else { break };
            let entry = entry?;
            let name: &CStr = entry.file_name();
            if name.to_bytes() == b"." || name.to_bytes() == b".." {
                continue;
            }
            names.push(OsStr::from_bytes(name.to_bytes()).to_os_string());
        }
        Ok(names)
    }

    pub(super) fn directory_identity_at_path(path: &Path) -> io::Result<DirectoryIdentity> {
        open_exact_directory(path).map(|(_, identity)| identity)
    }

    pub(super) fn child_directory_identity(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<DirectoryIdentity> {
        let stat = rfs::statat(parent, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) != FileType::Directory {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not a directory",
            ));
        }
        Ok(identity_from_stat(stat))
    }

    fn identity_from_stat(stat: rfs::Stat) -> DirectoryIdentity {
        DirectoryIdentity {
            device: stat.st_dev,
            inode: stat.st_ino,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn active_temp_is_not_swept_by_another_writer() {
        let root = test_root("active-temp");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let name = temp_name();
        let active = ActiveTemp::register(directory.inner.identity, &name);
        let mut file = platform::create_new_file(
            &directory.inner.handle,
            &directory.inner.path,
            OsStr::new(&name),
        )
        .expect("active temp");
        file.write_all(b"active").expect("active bytes");
        drop(file);

        directory.sweep_orphan_temps().expect("skip active temp");
        assert!(root.join(&name).is_file());

        drop(active);
        directory
            .sweep_orphan_temps_with(|_| false)
            .expect("sweep dead-owner orphan");
        assert!(!root.join(&name).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_read_rejects_sparse_substitution_before_allocation() {
        let root = test_root("bounded-read");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let file = platform::create_new_file(
            &directory.inner.handle,
            &directory.inner.path,
            OsStr::new("artifact.jar"),
        )
        .expect("artifact");
        file.set_len(MAX_MANAGED_READ_BYTES + 1)
            .expect("sparse length");
        drop(file);

        let error = directory
            .read_authenticated("artifact.jar", None, None)
            .expect_err("oversized file");
        assert!(matches!(error, LoaderError::Verify(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nested_authenticated_read_uses_canonical_held_capabilities() {
        let root = test_root("nested-authenticated-read");
        fs::create_dir_all(root.join("one/two")).expect("nested root");
        fs::write(root.join("one/two/artifact"), b"authenticated").expect("artifact");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let relative = ArtifactRelativePath::new("one/two/artifact").expect("relative path");
        let sha1: [u8; 20] = Sha1::digest(b"authenticated").into();

        assert_eq!(
            directory
                .read_relative_authenticated(&relative, Some(13), &sha1)
                .expect("authenticated nested read"),
            b"authenticated"
        );
        assert_eq!(
            directory
                .read_relative_authenticated(&relative, None, &sha1)
                .expect("SHA-only authenticated nested read"),
            b"authenticated"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn nested_authenticated_read_rejects_ancestor_and_final_links() {
        use std::os::unix::fs::symlink;

        let root = test_root("nested-authenticated-links");
        let outside = test_root("nested-authenticated-links-outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("artifact"), b"outside").expect("outside artifact");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let sha1: [u8; 20] = Sha1::digest(b"outside").into();

        symlink(&outside, root.join("ancestor")).expect("ancestor link");
        let ancestor = ArtifactRelativePath::new("ancestor/artifact").expect("ancestor path");
        assert!(
            directory
                .read_relative_authenticated(&ancestor, Some(7), &sha1)
                .is_err()
        );
        fs::create_dir(root.join("real")).expect("real directory");
        symlink(outside.join("artifact"), root.join("real/final")).expect("final link");
        let final_path = ArtifactRelativePath::new("real/final").expect("final path");
        assert!(
            directory
                .read_relative_authenticated(&final_path, Some(7), &sha1)
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn bounded_snapshot_records_exact_facts_and_diffs() {
        let root = test_root("tree-snapshot-diff");
        fs::create_dir_all(root.join("nested")).expect("nested");
        fs::write(root.join("nested/first"), b"first").expect("first");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let limits = ManagedTreeLimits::bounded_test(8, 4, 32, 64);
        let before = directory.snapshot_tree(limits).expect("before");
        let first = ArtifactRelativePath::new("nested/first").expect("first path");
        assert_eq!(before.files()[&first].size(), 5);
        assert_eq!(
            before.files()[&first].sha1(),
            &<[u8; 20]>::from(Sha1::digest(b"first"))
        );

        fs::write(root.join("nested/first"), b"changed").expect("changed");
        fs::write(root.join("added"), b"added").expect("added");
        let after = directory.snapshot_tree(limits).expect("after");
        let diff = before.diff(&after);
        assert!(!diff.is_empty());
        assert!(diff.modified_files().contains_key(&first));
        assert!(
            diff.added_files()
                .contains_key(&ArtifactRelativePath::new("added").expect("added path"))
        );
        assert!(diff.removed_files().is_empty());
        assert!(diff.added_directories().is_empty());
        assert!(diff.removed_directories().is_empty());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn bounded_snapshot_rejects_entry_depth_and_byte_overflow() {
        let root = test_root("tree-snapshot-bounds");
        fs::create_dir_all(root.join("a/b")).expect("nested");
        fs::write(root.join("one"), b"1234").expect("one");
        fs::write(root.join("two"), b"1234").expect("two");
        let directory = ManagedDir::open_root(&root).expect("managed root");

        for limits in [
            ManagedTreeLimits::bounded_test(3, 4, 8, 16),
            ManagedTreeLimits::bounded_test(8, 1, 8, 16),
            ManagedTreeLimits::bounded_test(8, 4, 3, 16),
            ManagedTreeLimits::bounded_test(8, 4, 8, 7),
        ] {
            assert!(directory.snapshot_tree(limits).is_err());
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_tree_limit_check_rejects_oversized_sparse_file_without_hashing() {
        let root = test_root("tree-live-bounds");
        fs::create_dir_all(&root).expect("root");
        let file = fs::File::create(root.join("growing-output")).expect("output");
        file.set_len(MAX_MANAGED_TREE_FILE_BYTES + 1)
            .expect("sparse length");
        drop(file);
        let directory = ManagedDir::open_root(&root).expect("managed root");

        assert!(
            directory
                .validate_tree_usage_no_links(ManagedTreeLimits::processor_stage())
                .is_err()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn retained_file_import_streams_and_reauthenticates_destination() {
        let source_root = test_root("managed-import-source");
        let destination_root = test_root("managed-import-destination");
        fs::create_dir_all(&source_root).expect("source root");
        fs::create_dir_all(&destination_root).expect("destination root");
        let bytes = b"authenticated retained source";
        let source_path = source_root.join("source");
        fs::write(&source_path, bytes).expect("source");
        let destination = ManagedDir::open_root(&destination_root).expect("destination");
        let relative = ArtifactRelativePath::new("nested/artifact.jar").expect("path");
        let sha1: [u8; 20] = Sha1::digest(bytes).into();

        destination
            .import_relative_authenticated(
                &relative,
                fs::File::open(source_path).expect("retained source"),
                bytes.len() as u64,
                sha1,
            )
            .await
            .expect("streamed import");
        assert_eq!(
            destination
                .read_relative_authenticated(&relative, Some(bytes.len() as u64), &sha1)
                .expect("authenticated destination"),
            bytes
        );
        let _ = fs::remove_dir_all(source_root);
        let _ = fs::remove_dir_all(destination_root);
    }

    #[test]
    fn processor_stage_snapshot_entry_bound_is_cleanup_reusable() {
        let limits = ManagedTreeLimits::processor_stage();
        assert!(limits.max_entries <= MAX_MANAGED_DIRECTORY_ENTRIES);
    }

    #[test]
    fn bounded_snapshot_rejects_post_capture_replacement() {
        let root = test_root("tree-snapshot-replacement");
        fs::create_dir_all(&root).expect("root");
        fs::write(root.join("artifact"), b"before").expect("artifact");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let limits = ManagedTreeLimits::bounded_test(4, 2, 32, 32);

        let error = directory
            .snapshot_tree_with(limits, || {
                fs::write(root.join("artifact"), b"after").map_err(LoaderError::Io)
            })
            .expect_err("replacement between captures");

        assert!(matches!(error, LoaderError::Verify(_)));
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[test]
    fn bounded_snapshot_rejects_aliases_links_and_unexpected_kinds() {
        use std::os::unix::fs::symlink;
        use std::os::unix::net::UnixListener;

        let limits = ManagedTreeLimits::bounded_test(8, 2, 32, 64);
        let alias_root = test_root("tree-snapshot-alias");
        fs::create_dir_all(&alias_root).expect("alias root");
        fs::write(alias_root.join("Name"), b"one").expect("first alias");
        fs::write(alias_root.join("name"), b"two").expect("second alias");
        assert!(
            ManagedDir::open_root(&alias_root)
                .expect("managed alias root")
                .snapshot_tree(limits)
                .is_err()
        );

        let link_root = test_root("tree-snapshot-link");
        fs::create_dir_all(&link_root).expect("link root");
        symlink(&alias_root, link_root.join("link")).expect("link");
        assert!(
            ManagedDir::open_root(&link_root)
                .expect("managed link root")
                .snapshot_tree(limits)
                .is_err()
        );

        let kind_root = test_root("tree-snapshot-kind");
        fs::create_dir_all(&kind_root).expect("kind root");
        let _listener = UnixListener::bind(kind_root.join("socket")).expect("socket");
        assert!(
            ManagedDir::open_root(&kind_root)
                .expect("managed kind root")
                .snapshot_tree(limits)
                .is_err()
        );
        let _ = fs::remove_dir_all(alias_root);
        let _ = fs::remove_dir_all(link_root);
        let _ = fs::remove_dir_all(kind_root);
    }

    #[test]
    fn inactive_current_process_temp_is_swept() {
        let root = test_root("inactive-current-temp");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let name = temp_name();
        fs::write(root.join(&name), b"cancelled").expect("cancelled temp");

        directory
            .sweep_orphan_temps_with(|_| true)
            .expect("inactive current-process sweep");

        assert!(!root.join(name).exists());
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn pending_temp_drop_unlinks_while_file_handle_is_still_open() {
        let root = test_root("pending-temp-open-handle");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let name = temp_name();
        let active = ActiveTemp::register(directory.inner.identity, &name);
        let file = platform::create_new_file(
            &directory.inner.handle,
            &directory.inner.path,
            OsStr::new(&name),
        )
        .expect("pending temp");
        let pending = PendingTemp::arm(directory.clone(), &name, active);

        drop(pending);
        assert!(!root.join(&name).exists());
        drop(file);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn ordinary_entries_do_not_consume_the_temp_sweep_bound() {
        let root = test_root("temp-sweep-ordinary-entries");
        fs::create_dir_all(&root).expect("root");
        for index in 0..=MAX_TEMP_SWEEP_ENTRIES {
            fs::write(root.join(format!("artifact-{index}")), b"retained").expect("artifact");
        }
        let directory = ManagedDir::open_root(&root).expect("managed root");

        directory
            .write_exact("result", b"installed")
            .await
            .expect("managed write");

        assert_eq!(fs::read(root.join("result")).expect("result"), b"installed");
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_entry_overflow_has_no_partial_effects() {
        let root = test_root("cleanup-entry-overflow");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let child = directory.open_or_create_child("stage").expect("stage");
        for index in 0..9 {
            fs::write(child.path().join(format!("artifact-{index}")), b"retained")
                .expect("artifact");
        }

        let error = child
            .clear_contents_bounded(8)
            .expect_err("overflow must fail before cleanup");

        assert!(matches!(error, LoaderError::Verify(_)));
        for index in 0..9 {
            assert_eq!(
                fs::read(child.path().join(format!("artifact-{index}")))
                    .expect("retained artifact"),
                b"retained"
            );
        }
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn nested_cleanup_overflow_has_no_partial_effects() {
        let root = test_root("nested-cleanup-entry-overflow");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let child = directory.open_or_create_child("stage").expect("stage");
        fs::write(child.path().join("top-level"), b"retained").expect("top-level artifact");
        let nested = child.open_or_create_child("nested").expect("nested");
        for index in 0..7 {
            fs::write(nested.path().join(format!("artifact-{index}")), b"retained")
                .expect("nested artifact");
        }

        let error = child
            .clear_contents_bounded(8)
            .expect_err("aggregate overflow must fail before cleanup");

        assert!(matches!(error, LoaderError::Verify(_)));
        assert_eq!(
            fs::read(child.path().join("top-level")).expect("retained top-level artifact"),
            b"retained"
        );
        for index in 0..7 {
            assert_eq!(
                fs::read(nested.path().join(format!("artifact-{index}")))
                    .expect("retained nested artifact"),
                b"retained"
            );
        }
        drop(nested);
        drop(child);
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_retains_only_admitted_directory_shells() {
        let root = test_root("cleanup-retained-shells");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let child = directory.open_or_create_child("stage").expect("stage");
        let nested = child.open_or_create_child("nested").expect("nested");
        fs::write(child.path().join("top-level"), b"owned").expect("top-level artifact");
        fs::write(nested.path().join("nested-artifact"), b"owned").expect("nested artifact");

        child.clear_owned_contents().expect("clear owned tree");

        assert!(root.join("stage").is_dir());
        assert!(root.join("stage/nested").is_dir());
        assert!(!root.join("stage/top-level").exists());
        assert!(!root.join("stage/nested/nested-artifact").exists());
        drop(nested);
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cleanup_rescan_rejects_raced_entries_in_retained_shells() {
        let root = test_root("cleanup-rescan-race");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let child = directory.open_or_create_child("stage").expect("stage");
        let nested = child.open_or_create_child("nested").expect("nested");
        fs::write(child.path().join("owned"), b"owned").expect("owned file");
        let mut budget = CleanupBudget { remaining: 8 };
        let plan = child.plan_cleanup(0, &mut budget).expect("cleanup plan");

        child.execute_cleanup(&plan).expect("execute cleanup plan");
        fs::write(child.path().join("raced-file"), b"raced").expect("raced file");
        let raced_file = child
            .validate_cleanup_result(&plan)
            .expect_err("raced file must prevent cleanup success");
        assert!(matches!(raced_file, LoaderError::Verify(_)));

        fs::remove_file(child.path().join("raced-file")).expect("remove raced file");
        fs::create_dir(nested.path().join("raced-directory")).expect("raced directory");
        let raced_directory = child
            .validate_cleanup_result(&plan)
            .expect_err("raced directory must prevent cleanup success");
        assert!(matches!(raced_directory, LoaderError::Verify(_)));

        drop(plan);
        drop(nested);
        drop(child);
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn reserved_temp_overflow_has_no_partial_sweep() {
        let root = test_root("reserved-temp-overflow");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let foreign_pid = std::process::id().wrapping_add(1);
        let names = (0..=MAX_TEMP_SWEEP_ENTRIES)
            .map(|index| format!("{TEMP_PREFIX}{foreign_pid}-{:x}-0", index + 1))
            .collect::<Vec<_>>();
        for name in &names {
            fs::write(root.join(name), b"foreign-active").expect("foreign temp");
        }

        let error = directory
            .sweep_orphan_temps_with(|_| false)
            .expect_err("reserved temp overflow must fail before sweeping");

        assert!(matches!(error, LoaderError::Verify(_)));
        for name in names {
            assert_eq!(
                fs::read(root.join(name)).expect("retained overflow temp"),
                b"foreign-active"
            );
        }
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn live_or_reused_pid_temp_is_preserved() {
        let root = test_root("live-owner-temp");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let owner_pid = std::process::id().wrapping_add(1);
        let name = format!("{TEMP_PREFIX}{owner_pid}-1-0");
        fs::write(root.join(&name), b"potentially-live").expect("live-owner temp");

        directory
            .sweep_orphan_temps_with(|pid| pid == owner_pid)
            .expect("live or reused PID must be retained");

        assert_eq!(
            fs::read(root.join(name)).expect("retained live-owner temp"),
            b"potentially-live"
        );
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn dead_owner_temp_is_swept() {
        let root = test_root("dead-owner-temp");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let owner_pid = std::process::id().wrapping_add(1);
        let name = format!("{TEMP_PREFIX}{owner_pid}-1-0");
        fs::write(root.join(&name), b"dead-owner").expect("dead-owner temp");

        directory
            .sweep_orphan_temps_with(|_| false)
            .expect("dead-owner temp sweep");

        assert!(!root.join(name).exists());
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn malformed_reserved_temp_name_fails_closed() {
        let root = test_root("malformed-temp");
        fs::create_dir_all(&root).expect("root");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let name = format!("{TEMP_PREFIX}malformed");
        fs::write(root.join(&name), b"unknown").expect("unknown temp");

        let error = directory
            .sweep_orphan_temps()
            .expect_err("malformed reserved temp must fail closed");

        assert!(matches!(error, LoaderError::Verify(_)));
        assert_eq!(
            fs::read(root.join(name)).expect("retained unknown temp"),
            b"unknown"
        );
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn replacement_cannot_redirect_version_write_outside_root() {
        use std::os::unix::fs::symlink;

        let root = test_root("replacement-root");
        let outside = test_root("replacement-outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        fs::write(outside.join("sentinel"), b"untouched").expect("sentinel");
        let managed = ManagedDir::open_root(&root).expect("managed root");
        let versions = managed.open_or_create_child("versions").expect("versions");
        let version = versions.open_or_create_child("loader").expect("version");
        let parked = versions.path().join("parked");
        fs::rename(version.path(), &parked).expect("park admitted version");
        symlink(&outside, version.path()).expect("replacement symlink");

        let error = version
            .write_exact("loader.json", b"authenticated")
            .await
            .expect_err("renamed capability must not report success");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert_eq!(
            fs::read(outside.join("sentinel")).expect("sentinel"),
            b"untouched"
        );
        assert!(!outside.join("loader.json").exists());
        assert_eq!(
            fs::read(parked.join("loader.json")).expect("anchored write"),
            b"authenticated"
        );
        let _ = fs::remove_file(version.path());
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn orphan_temp_symlink_cleanup_never_follows_target() {
        use std::os::unix::fs::symlink;

        let root = test_root("temp-link-root");
        let outside = test_root("temp-link-outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched").expect("sentinel");
        let directory = ManagedDir::open_root(&root).expect("managed root");
        let temp = temp_name();
        symlink(&sentinel, root.join(&temp)).expect("temp symlink");

        directory
            .sweep_orphan_temps_with(|_| false)
            .expect("sweep dead-owner temp link");

        assert!(!root.join(temp).exists());
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_fails_closed_after_child_name_replacement() {
        use std::os::unix::fs::symlink;

        let root = test_root("cleanup-root");
        let outside = test_root("cleanup-outside");
        fs::create_dir_all(&root).expect("root");
        fs::create_dir_all(&outside).expect("outside");
        let sentinel = outside.join("sentinel");
        fs::write(&sentinel, b"untouched").expect("sentinel");
        let parent = ManagedDir::open_root(&root).expect("managed root");
        let child = parent.open_or_create_child("stage").expect("stage");
        fs::write(child.path().join("owned"), b"owned").expect("owned file");
        let parked = root.join("parked");
        fs::rename(child.path(), &parked).expect("park stage");
        symlink(&outside, child.path()).expect("replacement link");

        let error = child
            .clear_owned_contents()
            .expect_err("replacement must fail revalidation");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert_eq!(fs::read(&sentinel).expect("sentinel"), b"untouched");
        assert_eq!(
            fs::read(parked.join("owned")).expect("retained admitted file"),
            b"owned"
        );
        let _ = fs::remove_file(root.join("stage"));
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[cfg(unix)]
    #[test]
    fn cleanup_never_removes_replacement_empty_directory() {
        let root = test_root("cleanup-directory-replacement");
        fs::create_dir_all(&root).expect("root");
        let parent = ManagedDir::open_root(&root).expect("managed root");
        let child = parent.open_or_create_child("stage").expect("stage");
        fs::write(child.path().join("owned"), b"owned").expect("owned file");
        let parked = root.join("parked");
        fs::rename(child.path(), &parked).expect("park stage");
        fs::create_dir(child.path()).expect("replacement directory");

        let error = child
            .clear_owned_contents()
            .expect_err("replacement must fail revalidation");

        assert!(matches!(error, LoaderError::Io(_) | LoaderError::Verify(_)));
        assert!(root.join("stage").is_dir());
        assert_eq!(
            fs::read(parked.join("owned")).expect("retained admitted file"),
            b"owned"
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn held_directory_handle_denies_namespace_replacement() {
        let root = test_root("windows-lock");
        fs::create_dir_all(&root).expect("root");
        let held = ManagedDir::open_root(&root).expect("held root");
        let moved = root.with_extension("moved");
        assert!(fs::rename(&root, &moved).is_err());
        drop(held);
        fs::rename(&root, &moved).expect("rename after release");
        fs::rename(&moved, &root).expect("restore");
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(windows)]
    #[test]
    fn windows_entry_kind_classifies_files_and_directories_without_following() {
        let root = test_root("windows-entry-kind");
        fs::create_dir_all(root.join("child")).expect("child directory");
        fs::write(root.join("artifact"), b"artifact").expect("artifact");
        let directory = ManagedDir::open_root(&root).expect("managed root");

        assert_eq!(
            platform::entry_kind(
                &directory.inner.handle,
                &directory.inner.path,
                OsStr::new("child")
            )
            .expect("directory kind"),
            Some(EntryKind::Directory)
        );
        assert_eq!(
            platform::entry_kind(
                &directory.inner.handle,
                &directory.inner.path,
                OsStr::new("artifact")
            )
            .expect("file kind"),
            Some(EntryKind::File)
        );
        drop(directory);
        let _ = fs::remove_dir_all(root);
    }

    fn test_root(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-managed-loader-{label}-{}-{nanos:x}",
            std::process::id()
        ))
    }
}

#[cfg(windows)]
mod platform {
    use super::EntryKind;
    use std::ffi::{OsStr, OsString};
    use std::fs;
    use std::io;
    use std::mem::size_of;
    use std::os::windows::ffi::OsStrExt;
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_DELETE, FILE_SHARE_READ,
        FILE_SHARE_WRITE, FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx, MoveFileExW,
    };

    pub(super) type DirectoryHandle = fs::File;
    pub(super) type FileIdentity = DirectoryIdentity;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(super) struct DirectoryIdentity {
        volume: u64,
        id: [u8; 16],
    }

    pub(super) fn directory_identity_binding(identity: DirectoryIdentity) -> String {
        let id = identity
            .id
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        format!("windows:{:016x}:{id}", identity.volume)
    }

    pub(super) fn open_exact_directory(
        path: &Path,
    ) -> io::Result<(DirectoryHandle, DirectoryIdentity)> {
        let file = open_no_follow(path, FILE_LIST_DIRECTORY | FILE_READ_ATTRIBUTES, true)?;
        let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0
            || basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY == 0
            || !standard.Directory
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not an exact directory",
            ));
        }
        let identity = directory_identity(&file)?;
        Ok((file, identity))
    }

    pub(super) fn open_child_directory(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<(DirectoryHandle, DirectoryIdentity)> {
        open_exact_directory(&parent_path.join(name))
    }

    pub(super) fn create_child_directory(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        fs::create_dir(parent_path.join(name))
    }

    pub(super) fn create_new_file(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .write(true)
            .create_new(true)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
        options.open(parent_path.join(name))
    }

    pub(super) fn open_file_read(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let file = open_no_follow(
            &parent_path.join(name),
            windows_sys::Win32::Foundation::GENERIC_READ,
            false,
        )?;
        let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        if basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
            || standard.Directory
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not an exact file",
            ));
        }
        Ok(file)
    }

    pub(super) fn open_file_read_write(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<fs::File> {
        let file = open_no_follow(
            &parent_path.join(name),
            windows_sys::Win32::Foundation::GENERIC_READ
                | windows_sys::Win32::Foundation::GENERIC_WRITE,
            false,
        )?;
        let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        if basic.FileAttributes & (FILE_ATTRIBUTE_REPARSE_POINT | FILE_ATTRIBUTE_DIRECTORY) != 0
            || standard.Directory
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "entry is not an exact file",
            ));
        }
        Ok(file)
    }

    pub(super) fn file_identity(file: &fs::File) -> io::Result<FileIdentity> {
        directory_identity(file)
    }

    pub(super) fn rename_entry(
        _from_parent: &DirectoryHandle,
        from_path: &Path,
        from: &OsStr,
        _to_parent: &DirectoryHandle,
        to_path: &Path,
        to: &OsStr,
    ) -> io::Result<()> {
        fs::rename(from_path.join(from), to_path.join(to))
    }

    pub(super) fn rename_entry_no_replace(
        _from_parent: &DirectoryHandle,
        from_path: &Path,
        from: &OsStr,
        _to_parent: &DirectoryHandle,
        to_path: &Path,
        to: &OsStr,
    ) -> io::Result<()> {
        let source = wide_path(&from_path.join(from));
        let destination = wide_path(&to_path.join(to));
        let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
        if result == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    pub(super) fn remove_file(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        fs::remove_file(parent_path.join(name))
    }

    pub(super) fn sync_directory(directory: &DirectoryHandle) -> io::Result<()> {
        directory.sync_all()
    }

    pub(super) fn entry_kind(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<Option<EntryKind>> {
        let path = parent_path.join(name);
        let file = match open_no_follow(&path, FILE_READ_ATTRIBUTES, true) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error),
        };
        let basic: FILE_BASIC_INFO = query(&file, FileBasicInfo)?;
        let standard: FILE_STANDARD_INFO = query(&file, FileStandardInfo)?;
        if basic.FileAttributes & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Ok(Some(EntryKind::Link));
        }
        if basic.FileAttributes & FILE_ATTRIBUTE_DIRECTORY != 0 || standard.Directory {
            Ok(Some(EntryKind::Directory))
        } else {
            Ok(Some(EntryKind::File))
        }
    }

    pub(super) fn entry_names(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        limit: usize,
    ) -> io::Result<Vec<OsString>> {
        fs::read_dir(parent_path)?
            .take(limit)
            .map(|entry| entry.map(|entry| entry.file_name()))
            .collect()
    }

    pub(super) fn directory_identity_at_path(path: &Path) -> io::Result<DirectoryIdentity> {
        open_exact_directory(path).map(|(_, identity)| identity)
    }

    pub(super) fn child_directory_identity(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<DirectoryIdentity> {
        open_exact_directory(&parent_path.join(name)).map(|(_, identity)| identity)
    }

    fn open_no_follow(path: &Path, access: u32, include_directories: bool) -> io::Result<fs::File> {
        let mut options = fs::OpenOptions::new();
        options
            .read(true)
            .access_mode(access)
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
            .custom_flags(
                FILE_FLAG_OPEN_REPARSE_POINT
                    | if include_directories {
                        FILE_FLAG_BACKUP_SEMANTICS
                    } else {
                        0
                    },
            );
        options.open(path)
    }

    fn wide_path(path: &Path) -> Vec<u16> {
        path.as_os_str().encode_wide().chain(Some(0)).collect()
    }

    fn directory_identity(file: &fs::File) -> io::Result<DirectoryIdentity> {
        let info: FILE_ID_INFO = query(file, FileIdInfo)?;
        Ok(DirectoryIdentity {
            volume: info.VolumeSerialNumber,
            id: info.FileId.Identifier,
        })
    }

    fn query<T: Default>(file: &fs::File, class: i32) -> io::Result<T> {
        let mut value = T::default();
        let size = u32::try_from(size_of::<T>())
            .map_err(|_| io::Error::other("Windows file information is too large"))?;
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                class,
                (&mut value as *mut T).cast(),
                size,
            )
        };
        if ok == 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(value)
        }
    }
}
