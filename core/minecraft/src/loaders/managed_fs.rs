use crate::artifact_path::{ArtifactRelativePath, validate_artifact_path_segment};
use crate::loaders::types::LoaderError;
use sha1::{Digest as _, Sha1};
use std::collections::HashSet;
use std::ffi::{OsStr, OsString};
use std::io::{self, Read, SeekFrom};
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
static TEMP_SEQUENCE: AtomicU64 = AtomicU64::new(0);
static ACTIVE_TEMPS: OnceLock<Mutex<HashSet<ActiveTempKey>>> = OnceLock::new();

#[derive(Clone)]
pub(crate) struct ManagedDir {
    inner: Arc<ManagedDirInner>,
}

impl std::fmt::Debug for ManagedDir {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedDir")
            .field("path", &self.inner.path)
            .finish_non_exhaustive()
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

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct ActiveTempKey {
    directory: platform::DirectoryIdentity,
    name: OsString,
}

struct ActiveTemp {
    key: ActiveTempKey,
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

    pub(crate) fn open_or_create_child(&self, name: &str) -> Result<Self, LoaderError> {
        validate_segment(name)?;
        match platform::open_child_directory(&self.inner.handle, &self.inner.path, OsStr::new(name))
        {
            Ok((handle, identity)) => self.child(name, handle, identity),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                platform::create_child_directory(
                    &self.inner.handle,
                    &self.inner.path,
                    OsStr::new(name),
                )?;
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

    pub(crate) fn open_child(&self, name: &str) -> Result<Self, LoaderError> {
        validate_segment(name)?;
        let (handle, identity) =
            platform::open_child_directory(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
        self.child(name, handle, identity)
    }

    pub(crate) fn open_child_if_exists(&self, name: &str) -> Result<Option<Self>, LoaderError> {
        validate_segment(name)?;
        match platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))? {
            None => Ok(None),
            Some(EntryKind::Directory) => self.open_child(name).map(Some),
            Some(_) => Err(LoaderError::Verify(
                "managed loader child is not an exact directory".to_string(),
            )),
        }
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

    pub(crate) async fn write_exact(&self, name: &str, bytes: &[u8]) -> Result<(), LoaderError> {
        validate_segment(name)?;
        let temp_name = temp_name();
        let _active = ActiveTemp::register(self.inner.identity, &temp_name);
        self.sweep_orphan_temps()?;
        let file = platform::create_new_file(
            &self.inner.handle,
            &self.inner.path,
            OsStr::new(&temp_name),
        )?;
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
            let _ =
                platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(&temp_name));
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
            let _ =
                platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(&temp_name));
            return Err(LoaderError::Io(error));
        }
        if self.read_bounded(name, bytes.len() as u64, true)? != bytes {
            let _ = platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name));
            return Err(LoaderError::Verify(
                "installed loader artifact differs from authenticated bytes".to_string(),
            ));
        }
        self.revalidate()
    }

    pub(crate) fn read_exact(&self, name: &str) -> Result<Vec<u8>, LoaderError> {
        self.read_bounded(name, MAX_MANAGED_READ_BYTES, false)
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

    fn read_bounded(
        &self,
        name: &str,
        limit: u64,
        require_exact_len: bool,
    ) -> Result<Vec<u8>, LoaderError> {
        validate_segment(name)?;
        let mut file =
            platform::open_file_read(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
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
        file.by_ref()
            .take(limit.saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > limit || (require_exact_len && bytes.len() as u64 != limit) {
            return Err(LoaderError::Verify(
                "managed loader artifact changed during bounded read".to_string(),
            ));
        }
        Ok(bytes)
    }

    pub(crate) fn remove_file(&self, name: &str) -> Result<(), LoaderError> {
        validate_segment(name)?;
        match platform::entry_kind(&self.inner.handle, &self.inner.path, OsStr::new(name))? {
            None => Err(LoaderError::Verify(
                "managed loader cleanup target is missing".to_string(),
            )),
            Some(EntryKind::File | EntryKind::Link) => {
                platform::remove_file(&self.inner.handle, &self.inner.path, OsStr::new(name))?;
                self.revalidate()
            }
            Some(_) => Err(LoaderError::Verify(
                "managed loader cleanup target is not a file".to_string(),
            )),
        }
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

    fn sweep_orphan_temps(&self) -> Result<(), LoaderError> {
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
            if owner_is_live(owner_pid) {
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

fn validate_segment(name: &str) -> Result<(), LoaderError> {
    validate_artifact_path_segment(name).map_err(|_| {
        LoaderError::Verify("managed loader path segment is not canonical".to_string())
    })
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

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(super) struct DirectoryIdentity {
        device: u64,
        inode: u64,
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

    pub(super) fn remove_file(
        parent: &DirectoryHandle,
        _parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        Ok(rfs::unlinkat(parent, name, AtFlags::empty())?)
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
    use std::io::Write as _;

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
            .read_exact("artifact.jar")
            .expect_err("oversized file");
        assert!(matches!(error, LoaderError::Verify(_)));
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
    use std::os::windows::fs::OpenOptionsExt;
    use std::os::windows::io::AsRawHandle;
    use std::path::Path;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_REPARSE_POINT, FILE_BASIC_INFO,
        FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_LIST_DIRECTORY, FILE_READ_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_STANDARD_INFO, FileBasicInfo, FileIdInfo, FileStandardInfo,
        GetFileInformationByHandleEx,
    };

    pub(super) type DirectoryHandle = fs::File;

    #[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
    pub(super) struct DirectoryIdentity {
        volume: u64,
        id: [u8; 16],
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
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
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

    pub(super) fn remove_file(
        _parent: &DirectoryHandle,
        parent_path: &Path,
        name: &OsStr,
    ) -> io::Result<()> {
        fs::remove_file(parent_path.join(name))
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
            .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE)
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
