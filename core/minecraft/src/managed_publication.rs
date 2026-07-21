use crate::loaders::types::LoaderError;
use crate::managed_fs::{
    ManagedDir, ManagedDirectoryIdentity, ManagedFileGuard, ManagedPersistentFile,
};
use crate::portable_path::PortableFileName;
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

const PUBLICATION_DIRECTORY: &str = ".axial-publication";
const PUBLICATION_LOCK_FILE: &str = "publication.lock";
const MAX_LIVE_PUBLICATION_ROOTS: usize = 64;
const MAX_BLOCKING_PUBLICATION_TASKS: usize = 4;
const CROSS_PROCESS_RETRY_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ManagedPriorFingerprint {
    Absent,
    ExistingFile { sha1: String, size: u64 },
}

impl ManagedPriorFingerprint {
    pub(crate) fn matches_source(&self, sha1: &str, size: u64) -> bool {
        matches!(
            self,
            Self::ExistingFile {
                sha1: prior_sha1,
                size: prior_size,
            } if *prior_size == size && prior_sha1 == sha1
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedCanonicalState {
    Absent,
    Source,
    Prior,
}

#[derive(Debug, thiserror::Error)]
#[error("managed publication data is invalid")]
pub(crate) struct ManagedPublicationDataError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedTargetPathError {
    PortableAlias,
    Access,
}

type RootMutex = tokio::sync::Mutex<()>;
type RootMutexRegistry = HashMap<ManagedDirectoryIdentity, Weak<RootMutex>>;

static ROOT_MUTEXES: OnceLock<Mutex<RootMutexRegistry>> = OnceLock::new();
static BLOCKING_PUBLICATION_TASKS: OnceLock<Arc<tokio::sync::Semaphore>> = OnceLock::new();

#[derive(Debug, thiserror::Error)]
pub(crate) enum ManagedPublicationError {
    #[error("managed publication root admission failed: {0}")]
    Admission(#[from] LoaderError),
    #[error("managed publication root capacity is exhausted")]
    RootCapacityExhausted,
    #[error("managed publication blocking task stopped unexpectedly")]
    BlockingTaskStopped,
    #[error("managed publication is changing")]
    ReadBusy,
}

pub(crate) struct ManagedRootPublicationLease {
    root: ManagedDir,
    publication_directory: ManagedDir,
    ownership: Arc<ManagedRootPublicationOwnership>,
}

struct ManagedRootPublicationOwnership {
    lock_file: Arc<ManagedPersistentFile>,
    _in_process_guard: tokio::sync::OwnedMutexGuard<()>,
}

#[derive(Clone)]
pub(crate) struct ManagedPublicationLifetimeGuard {
    _ownership: Arc<ManagedRootPublicationOwnership>,
}

pub(crate) enum ManagedRootPublicationReadLease {
    NoLane {
        root: ManagedDir,
    },
    Locked {
        root: ManagedDir,
        publication_directory: ManagedDir,
        lock_file: ManagedPersistentFile,
    },
}

impl std::fmt::Debug for ManagedRootPublicationLease {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagedRootPublicationLease")
            .finish_non_exhaustive()
    }
}

impl ManagedRootPublicationLease {
    pub(crate) async fn acquire(root: ManagedDir) -> Result<Self, ManagedPublicationError> {
        let identity_root = root.clone();
        let identity = run_publication_blocking(move || identity_root.identity())
            .await?
            .map_err(ManagedPublicationError::Admission)?;
        let root_mutex = root_mutex(identity)?;
        let in_process_guard = root_mutex.lock_owned().await;

        let setup_root = root.clone();
        let (publication_directory, lock_file) = run_publication_blocking(move || {
            setup_root.revalidate()?;
            let publication_directory = setup_root.open_or_create_child(PUBLICATION_DIRECTORY)?;
            let lock_file =
                publication_directory.open_or_create_persistent_file(PUBLICATION_LOCK_FILE)?;
            Ok::<_, LoaderError>((publication_directory, Arc::new(lock_file)))
        })
        .await?
        .map_err(ManagedPublicationError::Admission)?;
        loop {
            let attempt = Arc::clone(&lock_file);
            if run_publication_blocking(move || attempt.try_lock_exclusive())
                .await?
                .map_err(ManagedPublicationError::Admission)?
            {
                break;
            }
            tokio::time::sleep(CROSS_PROCESS_RETRY_INTERVAL).await;
        }
        let validation_root = root.clone();
        let validation_publication = publication_directory.clone();
        let validation_lock = Arc::clone(&lock_file);
        if let Err(error) = run_publication_blocking(move || {
            validation_root
                .revalidate()
                .and_then(|()| validation_publication.revalidate())
                .and_then(|()| validation_lock.revalidate())
        })
        .await?
        {
            let _ = lock_file.unlock();
            return Err(error.into());
        }

        Ok(Self {
            root,
            publication_directory,
            ownership: Arc::new(ManagedRootPublicationOwnership {
                lock_file,
                _in_process_guard: in_process_guard,
            }),
        })
    }

    pub(crate) fn root(&self) -> &ManagedDir {
        &self.root
    }

    pub(crate) fn publication_directory(&self) -> &ManagedDir {
        &self.publication_directory
    }

    pub(crate) fn lifetime_guard(&self) -> ManagedPublicationLifetimeGuard {
        ManagedPublicationLifetimeGuard {
            _ownership: Arc::clone(&self.ownership),
        }
    }

    pub(crate) fn revalidate(&self) -> Result<(), ManagedPublicationError> {
        self.root.revalidate()?;
        self.publication_directory.revalidate()?;
        self.ownership.lock_file.revalidate()?;
        Ok(())
    }
}

impl ManagedRootPublicationReadLease {
    pub(crate) fn acquire(root: ManagedDir) -> Result<Self, ManagedPublicationError> {
        Self::try_acquire(root)?.ok_or(ManagedPublicationError::ReadBusy)
    }

    fn try_acquire(root: ManagedDir) -> Result<Option<Self>, ManagedPublicationError> {
        root.revalidate()?;
        if !root.has_portably_exact_child_name(PUBLICATION_DIRECTORY)? {
            return Ok(Some(Self::NoLane { root }));
        }

        let publication_directory = root.open_child(PUBLICATION_DIRECTORY)?;
        if !publication_directory.has_portably_exact_child_name(PUBLICATION_LOCK_FILE)? {
            return Ok(None);
        }
        let lock_file = publication_directory.open_persistent_file(PUBLICATION_LOCK_FILE)?;
        if !lock_file.try_lock_shared()? {
            return Ok(None);
        }
        if let Err(error) = root
            .revalidate()
            .and_then(|()| publication_directory.revalidate())
            .and_then(|()| lock_file.revalidate())
        {
            let _ = lock_file.unlock();
            return Err(error.into());
        }

        Ok(Some(Self::Locked {
            root,
            publication_directory,
            lock_file,
        }))
    }

    pub(crate) fn revalidate(&self) -> Result<(), ManagedPublicationError> {
        match self {
            Self::NoLane { root } => {
                root.revalidate()?;
                if root.has_portably_exact_child_name(PUBLICATION_DIRECTORY)? {
                    return Err(ManagedPublicationError::ReadBusy);
                }
            }
            Self::Locked {
                root,
                publication_directory,
                lock_file,
            } => {
                root.revalidate()?;
                publication_directory.revalidate()?;
                lock_file.revalidate()?;
            }
        }
        Ok(())
    }

    pub(crate) fn root_identity(
        &self,
    ) -> Result<ManagedDirectoryIdentity, ManagedPublicationError> {
        let root = match self {
            Self::NoLane { root } | Self::Locked { root, .. } => root,
        };
        Ok(root.identity()?)
    }
}

pub(crate) async fn run_publication_blocking<F, R>(work: F) -> Result<R, ManagedPublicationError>
where
    F: FnOnce() -> R + Send + 'static,
    R: Send + 'static,
{
    let semaphore = BLOCKING_PUBLICATION_TASKS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(MAX_BLOCKING_PUBLICATION_TASKS)));
    let permit = Arc::clone(semaphore)
        .acquire_owned()
        .await
        .map_err(|_| ManagedPublicationError::BlockingTaskStopped)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| ManagedPublicationError::BlockingTaskStopped)
}

impl Drop for ManagedRootPublicationOwnership {
    fn drop(&mut self) {
        let _ = self.lock_file.unlock();
    }
}

impl Drop for ManagedRootPublicationReadLease {
    fn drop(&mut self) {
        if let Self::Locked { lock_file, .. } = self {
            let _ = lock_file.unlock();
        }
    }
}

pub(crate) fn valid_publication_sha1(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn valid_publication_nonce(value: &str) -> bool {
    value.len() == 32
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

#[cfg(unix)]
pub(crate) fn valid_publication_root_binding(value: &str) -> bool {
    let mut fields = value.split(':');
    fields.next() == Some("unix")
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_none()
}

#[cfg(windows)]
pub(crate) fn valid_publication_root_binding(value: &str) -> bool {
    let mut fields = value.split(':');
    fields.next() == Some("windows")
        && fields.next().is_some_and(valid_fixed_hex_16)
        && fields.next().is_some_and(|id| {
            id.len() == 32
                && id
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
        && fields.next().is_none()
}

fn valid_fixed_hex_16(value: &str) -> bool {
    value.len() == 16
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

pub(crate) fn bounded_marker_bytes<T: Serialize>(
    marker: &T,
    max_bytes: usize,
) -> Result<Vec<u8>, ManagedPublicationDataError> {
    let bytes = serde_json::to_vec(marker).map_err(|_| ManagedPublicationDataError)?;
    if max_bytes == 0 || bytes.len() > max_bytes {
        return Err(ManagedPublicationDataError);
    }
    Ok(bytes)
}

pub(crate) fn read_bounded_marker<T: DeserializeOwned + Serialize>(
    lane: &ManagedDir,
    name: &str,
    max_bytes: usize,
) -> Result<Option<(T, ManagedFileGuard)>, ManagedPublicationDataError> {
    let max_bytes = u64::try_from(max_bytes).map_err(|_| ManagedPublicationDataError)?;
    if max_bytes == 0 {
        return Err(ManagedPublicationDataError);
    }
    let Some(guard) = lane
        .inspect_regular_file(name)
        .map_err(|_| ManagedPublicationDataError)?
    else {
        return Ok(None);
    };
    if guard.size() == 0 || guard.size() > max_bytes {
        return Err(ManagedPublicationDataError);
    }
    let bytes = lane
        .read_guarded_file_bounded(name, &guard, max_bytes)
        .map_err(|_| ManagedPublicationDataError)?;
    let marker = serde_json::from_slice(&bytes).map_err(|_| ManagedPublicationDataError)?;
    if serde_json::to_vec(&marker).map_err(|_| ManagedPublicationDataError)? != bytes {
        return Err(ManagedPublicationDataError);
    }
    Ok(Some((marker, guard)))
}

pub(crate) fn exact_portable_names(
    directory: &ManagedDir,
    allowed: &[&str],
    max_entries: usize,
) -> Result<BTreeSet<String>, ManagedPublicationDataError> {
    let listing_bound = max_entries
        .checked_add(1)
        .ok_or(ManagedPublicationDataError)?;
    let entries = directory
        .entries_bounded(listing_bound)
        .map_err(|_| ManagedPublicationDataError)?;
    if entries.len() > max_entries {
        return Err(ManagedPublicationDataError);
    }
    let allowed_folded = allowed
        .iter()
        .map(|name| {
            PortableFileName::new_exact(name)
                .map(|portable| (portable.key(), *name))
                .map_err(|_| ManagedPublicationDataError)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let mut names = BTreeSet::new();
    let mut folded = BTreeSet::new();
    for entry in entries {
        let entry = entry.to_str().ok_or(ManagedPublicationDataError)?;
        let entry_folded = PortableFileName::new_exact(entry)
            .map_err(|_| ManagedPublicationDataError)?
            .key();
        let Some((_, exact)) = allowed_folded
            .iter()
            .find(|(allowed, _)| allowed == &entry_folded)
        else {
            return Err(ManagedPublicationDataError);
        };
        if entry != *exact || !folded.insert(entry_folded) {
            return Err(ManagedPublicationDataError);
        }
        names.insert(entry.to_string());
    }
    Ok(names)
}

pub(crate) fn validate_existing_managed_target_path(
    root: &ManagedDir,
    root_name: &str,
    relative_path: &str,
) -> Result<(), ManagedTargetPathError> {
    if !root
        .has_portably_exact_child_name(root_name)
        .map_err(|_| ManagedTargetPathError::PortableAlias)?
    {
        return Ok(());
    }
    let mut directory = root
        .open_child(root_name)
        .map_err(|_| ManagedTargetPathError::Access)?;
    let mut segments = relative_path.split('/').peekable();
    while let Some(segment) = segments.next() {
        let exists = directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedTargetPathError::PortableAlias)?;
        if !exists {
            break;
        }
        if segments.peek().is_some() {
            directory = directory
                .open_child(segment)
                .map_err(|_| ManagedTargetPathError::Access)?;
        }
    }
    Ok(())
}

pub(crate) fn open_managed_target_parent(
    root: &ManagedDir,
    root_name: &str,
    relative_path: &str,
) -> Result<Option<(ManagedDir, String)>, ManagedPublicationDataError> {
    if !root
        .has_portably_exact_child_name(root_name)
        .map_err(|_| ManagedPublicationDataError)?
    {
        return Ok(None);
    }
    let mut directory = root
        .open_child(root_name)
        .map_err(|_| ManagedPublicationDataError)?;
    let mut segments = relative_path.split('/').peekable();
    while let Some(segment) = segments.next() {
        if segments.peek().is_none() {
            directory
                .has_portably_exact_child_name(segment)
                .map_err(|_| ManagedPublicationDataError)?;
            return Ok(Some((directory, segment.to_string())));
        }
        if !directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedPublicationDataError)?
        {
            return Ok(None);
        }
        directory = directory
            .open_child(segment)
            .map_err(|_| ManagedPublicationDataError)?;
    }
    Err(ManagedPublicationDataError)
}

pub(crate) fn managed_directory_path_exists(
    root: &ManagedDir,
    path: &str,
) -> Result<bool, ManagedPublicationDataError> {
    let mut segments = path.split('/');
    let first = segments.next().ok_or(ManagedPublicationDataError)?;
    if !root
        .has_portably_exact_child_name(first)
        .map_err(|_| ManagedPublicationDataError)?
    {
        return Ok(false);
    }
    let mut directory = root
        .open_child(first)
        .map_err(|_| ManagedPublicationDataError)?;
    for segment in segments {
        if !directory
            .has_portably_exact_child_name(segment)
            .map_err(|_| ManagedPublicationDataError)?
        {
            return Ok(false);
        }
        directory = directory
            .open_child(segment)
            .map_err(|_| ManagedPublicationDataError)?;
    }
    Ok(true)
}

pub(crate) fn authenticate_guarded_publication_file(
    directory: &ManagedDir,
    name: &str,
    guard: &ManagedFileGuard,
    sha1: &str,
    size: u64,
    max_size: u64,
) -> Result<(), ManagedPublicationDataError> {
    if size > max_size
        || guard.size() != size
        || directory
            .sha1_guarded_file(name, guard, max_size)
            .map_err(|_| ManagedPublicationDataError)?
            != sha1
    {
        return Err(ManagedPublicationDataError);
    }
    Ok(())
}

pub(crate) fn committed_terminal_shape_is_valid(
    prior: &ManagedPriorFingerprint,
    source_sha1: &str,
    source_size: u64,
    canonical: ManagedCanonicalState,
    stage_present: bool,
    quarantine_present: bool,
) -> bool {
    if canonical != ManagedCanonicalState::Source {
        return false;
    }
    match prior {
        ManagedPriorFingerprint::Absent => !stage_present && !quarantine_present,
        ManagedPriorFingerprint::ExistingFile { .. }
            if prior.matches_source(source_sha1, source_size) =>
        {
            stage_present && !quarantine_present
        }
        ManagedPriorFingerprint::ExistingFile { .. } => !stage_present && quarantine_present,
    }
}

pub(crate) fn rollback_terminal_shape_is_reachable(
    prior: &ManagedPriorFingerprint,
    source_sha1: &str,
    source_size: u64,
    canonical: ManagedCanonicalState,
    stage_present: bool,
    quarantine_present: bool,
) -> bool {
    match prior {
        ManagedPriorFingerprint::Absent => {
            !quarantine_present
                && match canonical {
                    ManagedCanonicalState::Source => !stage_present,
                    ManagedCanonicalState::Absent => true,
                    ManagedCanonicalState::Prior => false,
                }
        }
        ManagedPriorFingerprint::ExistingFile { .. }
            if prior.matches_source(source_sha1, source_size) =>
        {
            !quarantine_present && canonical == ManagedCanonicalState::Source
        }
        ManagedPriorFingerprint::ExistingFile { .. } => match canonical {
            ManagedCanonicalState::Source => !stage_present && quarantine_present,
            ManagedCanonicalState::Absent => stage_present && quarantine_present,
            ManagedCanonicalState::Prior => !quarantine_present,
        },
    }
}

pub(crate) fn settled_terminal_shape_is_valid(
    committed: bool,
    prior: &ManagedPriorFingerprint,
    source_sha1: &str,
    source_size: u64,
    canonical: ManagedCanonicalState,
    quarantine_present: bool,
) -> bool {
    if committed {
        canonical == ManagedCanonicalState::Source
            && match prior {
                ManagedPriorFingerprint::Absent => !quarantine_present,
                ManagedPriorFingerprint::ExistingFile { .. }
                    if prior.matches_source(source_sha1, source_size) =>
                {
                    !quarantine_present
                }
                ManagedPriorFingerprint::ExistingFile { .. } => true,
            }
    } else {
        !quarantine_present
            && match prior {
                ManagedPriorFingerprint::Absent => canonical == ManagedCanonicalState::Absent,
                ManagedPriorFingerprint::ExistingFile { .. }
                    if prior.matches_source(source_sha1, source_size) =>
                {
                    canonical == ManagedCanonicalState::Source
                }
                ManagedPriorFingerprint::ExistingFile { .. } => {
                    canonical == ManagedCanonicalState::Prior
                }
            }
    }
}

fn root_mutex(
    identity: ManagedDirectoryIdentity,
) -> Result<Arc<RootMutex>, ManagedPublicationError> {
    let registry = ROOT_MUTEXES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut registry = registry
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    registry.retain(|_, root_mutex| root_mutex.strong_count() > 0);
    if let Some(root_mutex) = registry.get(&identity).and_then(Weak::upgrade) {
        return Ok(root_mutex);
    }
    if registry.len() >= MAX_LIVE_PUBLICATION_ROOTS {
        return Err(ManagedPublicationError::RootCapacityExhausted);
    }
    let root_mutex = Arc::new(RootMutex::new(()));
    registry.insert(identity, Arc::downgrade(&root_mutex));
    Ok(root_mutex)
}

#[cfg(test)]
mod tests {
    use super::{
        ManagedCanonicalState, ManagedPriorFingerprint, ManagedPublicationError,
        ManagedRootPublicationLease, ManagedRootPublicationReadLease, ManagedTargetPathError,
        PUBLICATION_DIRECTORY, PUBLICATION_LOCK_FILE, authenticate_guarded_publication_file,
        bounded_marker_bytes, committed_terminal_shape_is_valid, managed_directory_path_exists,
        open_managed_target_parent, read_bounded_marker, rollback_terminal_shape_is_reachable,
        settled_terminal_shape_is_valid, valid_publication_nonce, valid_publication_root_binding,
        valid_publication_sha1, validate_existing_managed_target_path,
    };
    use crate::managed_fs::ManagedDir;
    use serde::{Deserialize, Serialize};
    use sha1::{Digest as _, Sha1};
    use std::ffi::OsString;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    const PUBLICATION_ACQUIRE_TIMEOUT: Duration = Duration::from_secs(5);

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    #[serde(deny_unknown_fields)]
    struct TestMarker {
        value: u64,
    }

    #[test]
    fn publication_identifiers_are_strict_lowercase_hex() {
        assert!(valid_publication_sha1(&"a".repeat(40)));
        assert!(!valid_publication_sha1(&"A".repeat(40)));
        assert!(!valid_publication_sha1(&"a".repeat(39)));
        assert!(valid_publication_nonce(&"0".repeat(32)));
        assert!(!valid_publication_nonce(&"g".repeat(32)));

        #[cfg(unix)]
        {
            assert!(valid_publication_root_binding(
                "unix:0000000000000001:0000000000000002"
            ));
            assert!(!valid_publication_root_binding(
                "unix:0000000000000001:0000000000000002:extra"
            ));
        }
        #[cfg(windows)]
        {
            assert!(valid_publication_root_binding(
                "windows:0000000000000001:00000000000000000000000000000002"
            ));
            assert!(!valid_publication_root_binding(
                "windows:0000000000000001:0000000000000000000000000000000G"
            ));
        }
    }

    #[test]
    fn bounded_markers_require_exact_canonical_json() {
        let temporary = library_root("bounded-marker");
        let root = ManagedDir::open_root(&temporary.path().join("library")).expect("managed root");
        let lane = root.open_or_create_child("lane").expect("marker lane");
        let marker = TestMarker { value: 7 };
        let bytes = bounded_marker_bytes(&marker, 32).expect("bounded marker");
        assert_eq!(bytes, br#"{"value":7}"#);
        assert!(bounded_marker_bytes(&marker, bytes.len() - 1).is_err());

        lane.write_new_exact("marker.json", &bytes)
            .expect("write marker");
        let (read, _) = read_bounded_marker::<TestMarker>(&lane, "marker.json", 32)
            .expect("read marker")
            .expect("present marker");
        assert_eq!(read, marker);

        lane.write_new_exact("noncanonical.json", br#"{ "value": 7 }"#)
            .expect("write noncanonical marker");
        assert!(
            read_bounded_marker::<TestMarker>(&lane, "noncanonical.json", 32).is_err(),
            "equivalent but noncanonical JSON must fail closed"
        );
    }

    #[test]
    fn managed_target_traversal_rejects_portable_aliases() {
        let temporary = library_root("target-traversal");
        let root_path = temporary.path().join("library");
        fs::create_dir(root_path.join("libraries")).expect("libraries root");
        fs::create_dir(root_path.join("libraries/com")).expect("group root");
        fs::create_dir(root_path.join("libraries/com/example")).expect("artifact parent");
        let root = ManagedDir::open_root(&root_path).expect("managed root");

        validate_existing_managed_target_path(&root, "libraries", "com/example/library.jar")
            .expect("exact target path");
        let (parent, name) =
            open_managed_target_parent(&root, "libraries", "com/example/library.jar")
                .expect("target traversal")
                .expect("existing parent");
        assert_eq!(parent.path(), root_path.join("libraries/com/example"));
        assert_eq!(name, "library.jar");
        assert!(
            managed_directory_path_exists(&root, "libraries/com/example")
                .expect("existing directory")
        );

        let alias_temporary = library_root("target-alias");
        let alias_root_path = alias_temporary.path().join("library");
        fs::create_dir(alias_root_path.join("Libraries")).expect("aliased libraries root");
        let alias_root = ManagedDir::open_root(&alias_root_path).expect("aliased managed root");
        assert_eq!(
            validate_existing_managed_target_path(&alias_root, "libraries", "library.jar"),
            Err(ManagedTargetPathError::PortableAlias)
        );
    }

    #[test]
    fn guarded_authentication_binds_identity_size_and_digest() {
        let temporary = library_root("guarded-authentication");
        let root = ManagedDir::open_root(&temporary.path().join("library")).expect("managed root");
        let staging = root.open_or_create_child("staging").expect("staging root");
        let bytes = b"authenticated publication source";
        staging
            .write_new_exact("source", bytes)
            .expect("publication source");
        let guard = staging
            .inspect_regular_file("source")
            .expect("inspect source")
            .expect("source guard");
        let sha1 = format!("{:x}", Sha1::digest(bytes));

        authenticate_guarded_publication_file(
            &staging,
            "source",
            &guard,
            &sha1,
            bytes.len() as u64,
            1024,
        )
        .expect("authenticated guard");
        assert!(
            authenticate_guarded_publication_file(
                &staging,
                "source",
                &guard,
                &"0".repeat(40),
                bytes.len() as u64,
                1024,
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_shape_predicates_distinguish_absent_exact_and_replaced_priors() {
        let source_sha1 = "1".repeat(40);
        let other_sha1 = "2".repeat(40);
        let absent = ManagedPriorFingerprint::Absent;
        let exact = ManagedPriorFingerprint::ExistingFile {
            sha1: source_sha1.clone(),
            size: 7,
        };
        let replaced = ManagedPriorFingerprint::ExistingFile {
            sha1: other_sha1,
            size: 7,
        };
        assert_eq!(
            serde_json::to_value(&replaced).expect("serialize prior fingerprint"),
            serde_json::json!({
                "state": "existing_file",
                "sha1": "2222222222222222222222222222222222222222",
                "size": 7,
            })
        );

        assert!(committed_terminal_shape_is_valid(
            &absent,
            &source_sha1,
            7,
            ManagedCanonicalState::Source,
            false,
            false,
        ));
        assert!(committed_terminal_shape_is_valid(
            &exact,
            &source_sha1,
            7,
            ManagedCanonicalState::Source,
            true,
            false,
        ));
        assert!(committed_terminal_shape_is_valid(
            &replaced,
            &source_sha1,
            7,
            ManagedCanonicalState::Source,
            false,
            true,
        ));
        assert!(rollback_terminal_shape_is_reachable(
            &replaced,
            &source_sha1,
            7,
            ManagedCanonicalState::Absent,
            true,
            true,
        ));
        assert!(settled_terminal_shape_is_valid(
            false,
            &replaced,
            &source_sha1,
            7,
            ManagedCanonicalState::Prior,
            false,
        ));
        assert!(!settled_terminal_shape_is_valid(
            false,
            &absent,
            &source_sha1,
            7,
            ManagedCanonicalState::Source,
            false,
        ));
    }

    #[tokio::test]
    async fn same_root_aliases_share_in_process_exclusion() {
        let temporary = library_root("same-root-alias");
        let root = temporary.path().join("library");
        let first =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        let alias = ManagedDir::open_root(&root.join(".")).expect("root alias");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(alias));

        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        drop(first);
        waiter.await.expect("waiter task").expect("alias lease");
    }

    #[tokio::test]
    async fn lifetime_guard_retains_writer_exclusion_after_lease_drop() {
        let temporary = library_root("lifetime-guard");
        let root = temporary.path().join("library");
        let lease =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        let lifetime_guard = lease.lifetime_guard();
        drop(lease);

        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("waiting root"),
        ));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());

        drop(lifetime_guard);
        tokio::time::timeout(PUBLICATION_ACQUIRE_TIMEOUT, waiter)
            .await
            .expect("lifetime guard released writer")
            .expect("waiter task")
            .expect("waiting lease");
    }

    #[tokio::test]
    async fn different_roots_do_not_share_publication_exclusion() {
        let first_temporary = library_root("different-root-first");
        let second_temporary = library_root("different-root-second");
        let first = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&first_temporary.path().join("library")).expect("first root"),
        )
        .await
        .expect("first lease");

        let second = tokio::time::timeout(
            PUBLICATION_ACQUIRE_TIMEOUT,
            ManagedRootPublicationLease::acquire(
                ManagedDir::open_root(&second_temporary.path().join("library"))
                    .expect("second root"),
            ),
        )
        .await
        .expect("second root did not wait")
        .expect("second lease");

        drop((first, second));
    }

    #[test]
    fn reader_without_a_publication_lane_does_not_create_metadata() {
        let temporary = library_root("reader-no-lane");
        let root = temporary.path().join("library");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .expect("reader admission");

        assert!(!root.join(PUBLICATION_DIRECTORY).exists());
        reader.revalidate().expect("stable missing lane");
    }

    #[tokio::test]
    async fn publication_lane_appearing_during_an_unlocked_read_invalidates_it() {
        let temporary = library_root("reader-lane-race");
        let root = temporary.path().join("library");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("reader root"),
        )
        .expect("reader admission");
        let writer = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("writer root"),
        )
        .await
        .expect("writer admission");

        assert!(matches!(
            reader.revalidate(),
            Err(ManagedPublicationError::ReadBusy)
        ));
        drop(writer);
    }

    #[tokio::test]
    async fn existing_publication_lane_excludes_writer_while_reader_is_live() {
        let temporary = library_root("reader-shared-lock");
        let root = temporary.path().join("library");
        ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("initial writer root"),
        )
        .await
        .expect("initial writer admission");
        let reader = ManagedRootPublicationReadLease::acquire(
            ManagedDir::open_root(&root).expect("reader root"),
        )
        .expect("reader admission");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("waiting writer root"),
        ));

        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        reader.revalidate().expect("live reader");
        drop(reader);
        tokio::time::timeout(PUBLICATION_ACQUIRE_TIMEOUT, waiter)
            .await
            .expect("writer unblocked")
            .expect("writer task")
            .expect("writer admission");
    }

    #[tokio::test]
    async fn file_substitution_for_publication_directory_is_rejected() {
        let temporary = library_root("file-substitution");
        let root = temporary.path().join("library");
        fs::write(root.join(PUBLICATION_DIRECTORY), b"not a directory")
            .expect("publication substitution");

        let error = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect_err("file substitution must fail closed");
        assert!(matches!(error, ManagedPublicationError::Admission(_)));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn symlink_substitution_for_lock_file_is_rejected() {
        use std::os::unix::fs::symlink;

        let temporary = library_root("symlink-substitution");
        let root = temporary.path().join("library");
        let publication = root.join(PUBLICATION_DIRECTORY);
        fs::create_dir(&publication).expect("publication directory");
        fs::write(root.join("outside-lock"), b"").expect("outside lock");
        symlink(
            root.join("outside-lock"),
            publication.join(PUBLICATION_LOCK_FILE),
        )
        .expect("lock symlink");

        let error = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect_err("symlink substitution must fail closed");
        assert!(matches!(error, ManagedPublicationError::Admission(_)));
    }

    #[tokio::test]
    async fn locked_file_substitution_is_denied_or_detected_by_revalidation() {
        let temporary = library_root("lock-file-replacement");
        let root = temporary.path().join("library");
        let lease = ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("managed root"),
        )
        .await
        .expect("publication lease");
        let publication = root.join(PUBLICATION_DIRECTORY);
        let replacement = fs::rename(
            publication.join(PUBLICATION_LOCK_FILE),
            publication.join("displaced.lock"),
        );
        match replacement {
            Ok(()) => {
                fs::write(publication.join(PUBLICATION_LOCK_FILE), b"")
                    .expect("replacement lock file");
                assert!(matches!(
                    lease.revalidate(),
                    Err(ManagedPublicationError::Admission(_))
                ));
            }
            Err(_) => lease.revalidate().expect("locked identity remains exact"),
        }
    }

    #[tokio::test]
    async fn cancelling_waiter_releases_its_root_mutex_reference() {
        let temporary = library_root("cancelled-waiter");
        let root = temporary.path().join("library");
        let first =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        let waiting_root = ManagedDir::open_root(&root).expect("waiting root");
        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(waiting_root));
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(!waiter.is_finished());
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
        drop(first);

        tokio::time::timeout(
            PUBLICATION_ACQUIRE_TIMEOUT,
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("final root")),
        )
        .await
        .expect("cancelled waiter released exclusion")
        .expect("final lease");
    }

    #[tokio::test]
    async fn cancelling_cross_process_waiter_releases_in_process_exclusion() {
        let temporary = library_root("cancelled-cross-process-waiter");
        let root = temporary.path().join("library");
        let managed_root = ManagedDir::open_root(&root).expect("managed root");
        let publication = managed_root
            .open_or_create_child(PUBLICATION_DIRECTORY)
            .expect("publication directory");
        let external_lock = publication
            .open_or_create_persistent_file(PUBLICATION_LOCK_FILE)
            .expect("external lock file");
        assert!(external_lock.try_lock_exclusive().expect("external lock"));

        let waiter = tokio::spawn(ManagedRootPublicationLease::acquire(
            ManagedDir::open_root(&root).expect("waiting root"),
        ));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!waiter.is_finished());
        waiter.abort();
        assert!(waiter.await.expect_err("cancelled waiter").is_cancelled());
        external_lock.unlock().expect("release external lock");

        tokio::time::timeout(
            PUBLICATION_ACQUIRE_TIMEOUT,
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("final root")),
        )
        .await
        .expect("cancelled cross-process waiter released exclusion")
        .expect("final lease");
    }

    #[tokio::test]
    async fn lock_lane_is_fixed_and_persistent() {
        let temporary = library_root("persistent-lane");
        let root = temporary.path().join("library");
        let lease =
            ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("first root"))
                .await
                .expect("first lease");
        lease.revalidate().expect("live lease");
        assert_eq!(
            lease.root().identity().expect("lease root identity"),
            ManagedDir::open_root(&root)
                .expect("reopened root")
                .identity()
                .expect("reopened root identity")
        );
        assert_eq!(
            lease.publication_directory().path(),
            root.join(PUBLICATION_DIRECTORY)
        );
        drop(lease);

        let entries = fs::read_dir(root.join(PUBLICATION_DIRECTORY))
            .expect("persistent publication directory")
            .map(|entry| entry.expect("publication entry").file_name())
            .collect::<Vec<_>>();
        assert_eq!(entries, [OsString::from(PUBLICATION_LOCK_FILE)]);

        ManagedRootPublicationLease::acquire(ManagedDir::open_root(&root).expect("second root"))
            .await
            .expect("persistent lane reacquired");
        assert!(
            root.join(PUBLICATION_DIRECTORY)
                .join(PUBLICATION_LOCK_FILE)
                .is_file()
        );
    }

    fn library_root(label: &str) -> TempDir {
        let temporary = tempfile::Builder::new()
            .prefix(&format!("axial-managed-publication-{label}-"))
            .tempdir()
            .expect("temporary root");
        fs::create_dir(temporary.path().join("library")).expect("library root");
        temporary
    }
}
