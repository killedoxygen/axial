//! Vanilla materialization through the retained, capability-scoped Minecraft installer.
//!
//! The returned receipt proves the published payload, not application readiness. The
//! queue must bind it to the durable publication evidence, persist its activation
//! source, and acknowledge publication before exposing a ready installation.

use axial_minecraft::download::{
    ExecutionDownloadFact, ExpectedIntegrity, LibraryVerificationIntegrity,
    library_verification_plans_for,
};
use axial_minecraft::managed_path::{
    ManagedLibraryFile, ManagedLibraryOperation, ManagedNativeDirectory,
};
use axial_minecraft::portable_path::{PortableFileName, PortableRelativePath};
use axial_minecraft::rules::Environment;
use axial_minecraft::runtime::ManagedRuntimeCache;
use axial_minecraft::{
    DownloadError, DownloadProgress, Downloader, KnownGoodInstallReceipt, ResolvedLibrary,
    VersionJson,
};
use serde::Serialize;
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{Cursor, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;

const MAX_NATIVE_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_NATIVE_FILE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_NATIVE_TOTAL_BYTES: u64 = 512 * 1024 * 1024;
const MAX_NATIVE_ENTRIES: usize = 16_384;

/// One published, verified native directory and the exact source archives it
/// was extracted from. Immutable while any session uses it.
struct NativeTree {
    directory: ManagedNativeDirectory,
    path: PathBuf,
    sources: Vec<NativeSource>,
}

impl NativeTree {
    /// The directory's exact membership and file guards, and every source
    /// archive's revision, still match what was extracted and published.
    fn revalidate(&self) -> Result<(), NativePreparationError> {
        self.directory
            .revalidate()
            .map_err(|_| NativePreparationError::Changed)?;
        for source in &self.sources {
            source
                .file
                .revalidate()
                .map_err(|_| NativePreparationError::Changed)?;
        }
        Ok(())
    }
}

/// Native directories retained in this process for reuse by later sessions.
const MAX_RETAINED_NATIVES: usize = 4;
/// How long an unused retained directory is kept before it is settled.
const NATIVE_IDLE_RETENTION: std::time::Duration = std::time::Duration::from_secs(300);

struct RetainedNatives {
    key: [u8; 32],
    tree: Arc<NativeTree>,
    users: usize,
    idle_since: Option<std::time::Instant>,
    /// Settlement failed (its files no longer match their proof); the exact
    /// obligation stays owned here and the tree is never reused.
    unresolved: bool,
}

/// Bounded reuse of verified native directories across this process's
/// sessions. An entry is reused only after its exact proof is rechecked;
/// unused entries are settled when evicted, after `NATIVE_IDLE_RETENTION`,
/// or when the owner releases them (shutdown). Nothing persists across a
/// restart.
#[derive(Clone, Default)]
pub(crate) struct NativeRetention {
    entries: Arc<std::sync::Mutex<Vec<RetainedNatives>>>,
}

impl NativeRetention {
    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<RetainedNatives>> {
        self.entries
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A retained tree for `key`, if its proof still holds. Blocking I/O.
    fn reuse(&self, key: &[u8; 32]) -> Option<Arc<NativeTree>> {
        let candidate = self
            .lock()
            .iter()
            .find(|entry| &entry.key == key && !entry.unresolved)
            .map(|entry| entry.tree.clone())?;
        if candidate.revalidate().is_err() {
            return None;
        }
        let mut entries = self.lock();
        let entry = entries
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.tree, &candidate))?;
        entry.users += 1;
        entry.idle_since = None;
        Some(candidate)
    }

    /// Records a fresh tree in use and returns idle entries over capacity,
    /// which the caller settles.
    fn insert(&self, key: [u8; 32], tree: Arc<NativeTree>) -> Vec<([u8; 32], Arc<NativeTree>)> {
        let mut entries = self.lock();
        let mut evicted = Vec::new();
        // An idle entry for the same inputs failed its proof; settle it.
        entries.retain(|entry| {
            let replaced = entry.key == key && entry.users == 0 && !entry.unresolved;
            if replaced {
                evicted.push((entry.key, entry.tree.clone()));
            }
            !replaced
        });
        entries.push(RetainedNatives {
            key,
            tree,
            users: 1,
            idle_since: None,
            unresolved: false,
        });
        while entries.len() > MAX_RETAINED_NATIVES {
            let Some(oldest) = entries
                .iter()
                .enumerate()
                .filter(|(_, entry)| entry.users == 0 && !entry.unresolved)
                .min_by_key(|(_, entry)| entry.idle_since)
                .map(|(index, _)| index)
            else {
                break;
            };
            let entry = entries.remove(oldest);
            evicted.push((entry.key, entry.tree));
        }
        evicted
    }

    /// Ends one session's use. True when the tree stays retained for reuse;
    /// false when the caller owns its settlement.
    fn release(&self, tree: &Arc<NativeTree>) -> bool {
        let mut entries = self.lock();
        let Some(entry) = entries
            .iter_mut()
            .find(|entry| Arc::ptr_eq(&entry.tree, tree))
        else {
            return false;
        };
        entry.users = entry.users.saturating_sub(1);
        if entry.users == 0 {
            entry.idle_since = Some(std::time::Instant::now());
            drop(entries);
            self.schedule_expiry();
        }
        true
    }

    fn schedule_expiry(&self) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let retention = self.clone();
        runtime.spawn(async move {
            tokio::time::sleep(NATIVE_IDLE_RETENTION).await;
            let _ = tokio::task::spawn_blocking(move || {
                retention.settle_idle(Some(NATIVE_IDLE_RETENTION))
            })
            .await;
        });
    }

    /// Settles unused entries (older than `older_than`, or all). An entry
    /// whose settlement fails stays retained with its exact obligation.
    /// Blocking I/O.
    pub(crate) fn settle_idle(&self, older_than: Option<std::time::Duration>) -> std::io::Result<()> {
        let idle = {
            let mut entries = self.lock();
            let mut idle = Vec::new();
            entries.retain(|entry| {
                let expired = entry.users == 0
                    && entry.idle_since.is_some_and(|since| {
                        older_than.is_none_or(|limit| since.elapsed() >= limit)
                    });
                if expired {
                    idle.push((entry.key, entry.tree.clone()));
                }
                !expired
            });
            idle
        };
        let mut failure = None;
        for (key, tree) in idle {
            if let Err(error) = tree.directory.settle() {
                self.retain_unresolved(key, tree);
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }

    /// Keeps a tree whose settlement failed as an owned, unusable obligation.
    fn retain_unresolved(&self, key: [u8; 32], tree: Arc<NativeTree>) {
        self.lock().push(RetainedNatives {
            key,
            tree,
            users: 0,
            idle_since: Some(std::time::Instant::now()),
            unresolved: true,
        });
    }

    /// One settlement attempt for an evicted tree, off the async thread; a
    /// failure is retained here rather than retried in the launch path.
    async fn settle_evicted(&self, key: [u8; 32], tree: Arc<NativeTree>) {
        let attempt = tree.clone();
        if !matches!(
            tokio::task::spawn_blocking(move || attempt.directory.settle()).await,
            Ok(Ok(()))
        ) {
            self.retain_unresolved(key, tree);
        }
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        self.lock().len()
    }
}

/// One session's use of verified native files. Settling releases the use;
/// the files are removed when no session uses them and retention ends.
pub(crate) struct PreparedNatives {
    tree: Arc<NativeTree>,
    retention: Option<NativeRetention>,
    released: std::sync::atomic::AtomicBool,
}

impl PreparedNatives {
    /// Invoke only after the process tree and output have settled. A failed
    /// cleanup leaves this receipt owning the remaining exact files for retry.
    pub(crate) fn settle(&self) -> std::io::Result<()> {
        use std::sync::atomic::Ordering;
        if self.released.load(Ordering::Acquire) {
            return Ok(());
        }
        if self
            .retention
            .as_ref()
            .is_some_and(|retention| retention.release(&self.tree))
        {
            self.released.store(true, Ordering::Release);
            return Ok(());
        }
        self.tree.directory.settle()?;
        self.released.store(true, Ordering::Release);
        Ok(())
    }

    pub(crate) fn path(&self) -> &Path {
        &self.tree.path
    }

    pub(crate) fn revalidate(&self) -> Result<(), NativePreparationError> {
        self.tree.revalidate()
    }

    pub(crate) fn validate_sources(
        &self,
        libraries: &[ResolvedLibrary],
    ) -> Result<(), NativePreparationError> {
        let selected = libraries
            .iter()
            .filter(|library| library.is_native)
            .map(|library| (library.abs_path.clone(), library.name.clone()))
            .collect::<BTreeSet<_>>();
        let prepared = self
            .tree
            .sources
            .iter()
            .map(|source| (source.path.clone(), source.name.clone()))
            .collect::<BTreeSet<_>>();
        if selected != prepared {
            return Err(NativePreparationError::SourcesMismatch);
        }
        self.revalidate()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, thiserror::Error)]
pub(crate) enum NativePreparationError {
    #[error("Native library files are missing. Install this version before launching.")]
    Missing,
    #[error(
        "Native library integrity could not be verified. Install this version before launching."
    )]
    Integrity,
    #[error("A native library archive is invalid or contains unsafe entries.")]
    Archive,
    #[error("Native libraries contain conflicting extracted files.")]
    ConflictingFiles,
    #[error("Native libraries exceed the supported extraction limits.")]
    Limit,
    #[error("The selected native libraries changed during preparation.")]
    Changed,
    #[error("Prepared native libraries do not match the selected version.")]
    SourcesMismatch,
    #[error("The native library directory could not be prepared.")]
    Publication,
}

struct NativeSource {
    file: ManagedLibraryFile,
    path: PathBuf,
    name: String,
}

#[derive(Serialize)]
struct NativeManifest {
    schema: u32,
    sources: Vec<NativeManifestSource>,
    files: Vec<NativeManifestFile>,
}

#[derive(Serialize)]
struct NativeManifestSource {
    path: String,
    sha1: String,
    size: u64,
}

#[derive(Serialize)]
struct NativeManifestFile {
    name: String,
    sha1: String,
    size: u64,
}

struct NativeExtraction {
    sources: Vec<NativeSource>,
    files: Vec<(PortableFileName, Vec<u8>)>,
    manifest: Vec<u8>,
}

/// Prepare the selected native JARs before entering the pure launch planner.
/// The path argument is accepted only as a projection of `library`. With a
/// retention, an earlier verified extraction of the exact same inputs is
/// reused after its proof is rechecked; otherwise a fresh one is published.
pub(crate) async fn prepare_natives_with_exact_files(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    exact: Arc<BTreeMap<String, (String, u64)>>,
    retention: Option<NativeRetention>,
) -> Result<Option<PreparedNatives>, NativePreparationError> {
    library
        .validate_read_projection(library_root)
        .map_err(|_| NativePreparationError::Changed)?;
    enum Planned {
        None,
        Reused(Arc<NativeTree>),
        Fresh([u8; 32], NativeExtraction),
    }
    let root = library_root.to_owned();
    let operation = library.clone();
    let version = version.clone();
    let environment = environment.clone();
    let reuse = retention.clone();
    let planned = tokio::task::spawn_blocking(move || {
        let Some(selected) = select_native_sources(&root, &version, &environment, &exact)? else {
            return Ok(Planned::None);
        };
        let key = native_key(&selected);
        if let Some(tree) = reuse.as_ref().and_then(|retention| retention.reuse(&key)) {
            return Ok(Planned::Reused(tree));
        }
        Ok(match plan_native_extraction(&operation, &root, &version, &environment, &exact)? {
            Some(extraction) => Planned::Fresh(key, extraction),
            None => Planned::None,
        })
    })
    .await
    .map_err(|_| NativePreparationError::Archive)??;
    let (key, extraction) = match planned {
        Planned::None => return Ok(None),
        Planned::Reused(tree) => {
            return Ok(Some(PreparedNatives {
                tree,
                retention,
                released: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        Planned::Fresh(key, extraction) => (key, extraction),
    };

    // All sources were authenticated and all archive entries checked before any
    // publication. The leaf creates a new generated directory, never adopting a
    // marker or overwriting an existing cache directory.
    let mut files = extraction.files;
    files.push((
        PortableFileName::new_exact(".axial-native-manifest.json")
            .map_err(|_| NativePreparationError::Publication)?,
        extraction.manifest,
    ));
    let directory = match library.publish_native_directory(files).await {
        Ok(directory) => directory,
        Err(failure) => {
            let (_, directory) = failure.into_parts();
            if let Some(directory) = directory {
                settle_native_directory(directory).await;
            }
            return Err(NativePreparationError::Publication);
        }
    };
    let tree = verify_native_tree(NativeTree {
        path: directory.relative_path().join_under(library_root),
        directory,
        sources: extraction.sources,
    })
    .await?;
    let tree = Arc::new(tree);
    if let Some(retention) = &retention {
        for (evicted_key, evicted) in retention.insert(key, tree.clone()) {
            retention.settle_evicted(evicted_key, evicted).await;
        }
    }
    Ok(Some(PreparedNatives {
        tree,
        retention,
        released: std::sync::atomic::AtomicBool::new(false),
    }))
}

async fn verify_native_tree(tree: NativeTree) -> Result<NativeTree, NativePreparationError> {
    if let Err(error) = tree.revalidate() {
        // A changed source does not release ownership of the extraction that
        // was already published. Accepted launch preparation keeps its task and
        // admission until this exact receipt has settled, including retries.
        settle_native_directory(tree.directory).await;
        return Err(error);
    }
    Ok(tree)
}

async fn settle_native_directory(directory: ManagedNativeDirectory) {
    let directory = Arc::new(directory);
    loop {
        let retained = directory.clone();
        if matches!(
            tokio::task::spawn_blocking(move || retained.settle()).await,
            Ok(Ok(()))
        ) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

struct SelectedNative {
    library: ResolvedLibrary,
    relative: PortableRelativePath,
    expected: ExpectedIntegrity,
}

/// The native archives this version selects for `environment`, each with the
/// exact integrity it must have. Reads no file.
fn select_native_sources(
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    exact: &BTreeMap<String, (String, u64)>,
) -> Result<Option<Vec<SelectedNative>>, NativePreparationError> {
    let selected = axial_minecraft::resolve_libraries(version, library_root, environment)
        .into_iter()
        .filter(|library| library.is_native)
        .collect::<Vec<_>>();
    if selected.is_empty() {
        return Ok(None);
    }
    if selected.len() > MAX_NATIVE_ENTRIES {
        return Err(NativePreparationError::Limit);
    }
    let verification =
        library_verification_plans_for(library_root, &version.libraries, environment)
            .map_err(|_| NativePreparationError::Integrity)?
            .into_iter()
            .map(|plan| (plan.path, plan.integrity))
            .collect::<BTreeMap<_, _>>();
    selected
        .into_iter()
        .map(|library| {
            let relative = library
                .abs_path
                .strip_prefix(library_root)
                .ok()
                .and_then(|path| PortableRelativePath::from_path(path).ok())
                .ok_or(NativePreparationError::Integrity)?;
            let expected = match verification.get(&library.abs_path) {
                Some(LibraryVerificationIntegrity::Sha1(expected)) => expected.clone(),
                Some(LibraryVerificationIntegrity::MissingChecksum) => {
                    let (sha1, size) = exact
                        .get(relative.as_str())
                        .ok_or(NativePreparationError::Integrity)?;
                    ExpectedIntegrity {
                        size: Some(*size),
                        sha1: Some(sha1.clone()),
                    }
                }
                None => return Err(NativePreparationError::Integrity),
            };
            Ok(SelectedNative {
                library,
                relative,
                expected,
            })
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Some)
}

/// The exact identity of an extraction's inputs: every selected archive's
/// path and required integrity. Equal keys extract to identical files.
fn native_key(selected: &[SelectedNative]) -> [u8; 32] {
    use sha2::Digest as _;
    let mut digest = sha2::Sha256::new();
    digest.update(b"axial-native-inputs-v1");
    let mut entries = selected
        .iter()
        .map(|source| {
            (
                source.relative.as_str(),
                source.expected.sha1.as_deref().unwrap_or_default(),
                source.expected.size.unwrap_or_default(),
                source.library.name.as_str(),
            )
        })
        .collect::<Vec<_>>();
    entries.sort_unstable();
    for (path, sha1, size, name) in entries {
        for part in [path.as_bytes(), sha1.as_bytes(), name.as_bytes()] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
        digest.update(size.to_le_bytes());
    }
    digest.finalize().into()
}

fn plan_native_extraction(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    exact: &BTreeMap<String, (String, u64)>,
) -> Result<Option<NativeExtraction>, NativePreparationError> {
    library
        .validate_read_projection(library_root)
        .map_err(|_| NativePreparationError::Changed)?;
    let Some(selected) = select_native_sources(library_root, version, environment, exact)? else {
        return Ok(None);
    };
    let mut sources = Vec::new();
    let mut manifest_sources = Vec::new();
    let mut extracted = BTreeMap::new();
    let mut archive_bytes = 0_u64;
    let mut expanded_bytes = 0_u64;
    let mut entry_count = 0_usize;
    for SelectedNative {
        library: selected,
        relative,
        expected,
    } in selected
    {
        let file = library
            .observe_file(&relative)
            .map_err(|_| NativePreparationError::Changed)?
            .ok_or(NativePreparationError::Missing)?;
        archive_bytes = archive_bytes
            .checked_add(file.size())
            .ok_or(NativePreparationError::Limit)?;
        if file.size() > MAX_NATIVE_ARCHIVE_BYTES || archive_bytes > MAX_NATIVE_TOTAL_BYTES {
            return Err(NativePreparationError::Limit);
        }
        let bytes = file
            .read_bounded(MAX_NATIVE_ARCHIVE_BYTES)
            .map_err(|_| NativePreparationError::Changed)?;
        let digest = format!("{:x}", Sha1::digest(&bytes));
        if expected.size.is_some_and(|size| size != bytes.len() as u64)
            || !expected
                .sha1
                .as_ref()
                .is_some_and(|sha1| digest.eq_ignore_ascii_case(sha1))
        {
            return Err(NativePreparationError::Integrity);
        }
        extract_native_archive(bytes, &mut extracted, &mut entry_count, &mut expanded_bytes)?;
        manifest_sources.push(NativeManifestSource {
            path: relative.as_str().to_string(),
            sha1: digest,
            size: file.size(),
        });
        sources.push(NativeSource {
            file,
            path: selected.abs_path,
            name: selected.name,
        });
    }
    if extracted.is_empty() {
        return Err(NativePreparationError::Archive);
    }
    let files = extracted.into_values().collect::<Vec<_>>();
    let manifest_files = files
        .iter()
        .map(|(name, bytes)| NativeManifestFile {
            name: name.as_str().to_string(),
            sha1: format!("{:x}", Sha1::digest(bytes)),
            size: bytes.len() as u64,
        })
        .collect();
    let manifest = serde_json::to_vec(&NativeManifest {
        schema: 1,
        sources: manifest_sources,
        files: manifest_files,
    })
    .map_err(|_| NativePreparationError::Archive)?;
    for source in &sources {
        source
            .file
            .revalidate()
            .map_err(|_| NativePreparationError::Changed)?;
    }
    Ok(Some(NativeExtraction {
        sources,
        files,
        manifest,
    }))
}

fn extract_native_archive(
    bytes: Vec<u8>,
    extracted: &mut BTreeMap<String, (PortableFileName, Vec<u8>)>,
    entry_count: &mut usize,
    expanded_bytes: &mut u64,
) -> Result<(), NativePreparationError> {
    let mut archive =
        zip::ZipArchive::new(Cursor::new(bytes)).map_err(|_| NativePreparationError::Archive)?;
    *entry_count = entry_count
        .checked_add(archive.len())
        .ok_or(NativePreparationError::Limit)?;
    if *entry_count > MAX_NATIVE_ENTRIES {
        return Err(NativePreparationError::Limit);
    }
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|_| NativePreparationError::Archive)?;
        let spelling = entry
            .name()
            .map_err(|_| NativePreparationError::Archive)?
            .replace('\\', "/");
        let name = spelling.strip_suffix('/').unwrap_or(&spelling);
        let relative =
            PortableRelativePath::new_exact(name).map_err(|_| NativePreparationError::Archive)?;
        let mode = entry.unix_mode().map(|mode| mode & 0o170000);
        if mode.is_some_and(|mode| !matches!(mode, 0 | 0o040000 | 0o100000)) {
            return Err(NativePreparationError::Archive);
        }
        if entry.is_dir() || spelling.starts_with("META-INF/") {
            continue;
        }
        let filename = relative
            .as_str()
            .rsplit('/')
            .next()
            .and_then(|name| PortableFileName::new_exact(name).ok())
            .ok_or(NativePreparationError::Archive)?;
        if filename
            .as_str()
            .eq_ignore_ascii_case(".axial-native-manifest.json")
        {
            return Err(NativePreparationError::Archive);
        }
        if entry.size() > MAX_NATIVE_FILE_BYTES {
            return Err(NativePreparationError::Limit);
        }
        *expanded_bytes = expanded_bytes
            .checked_add(entry.size())
            .ok_or(NativePreparationError::Limit)?;
        if *expanded_bytes > MAX_NATIVE_TOTAL_BYTES {
            return Err(NativePreparationError::Limit);
        }
        let declared_size = entry.size();
        let mut content = Vec::new();
        entry
            .by_ref()
            .take(declared_size + 1)
            .read_to_end(&mut content)
            .map_err(|_| NativePreparationError::Archive)?;
        if content.len() as u64 != declared_size {
            return Err(NativePreparationError::Archive);
        }
        let key = filename.key().as_str().to_owned();
        if let Some((existing_name, existing_bytes)) = extracted.get(&key) {
            if existing_name != &filename || existing_bytes != &content {
                return Err(NativePreparationError::ConflictingFiles);
            }
        } else {
            extracted.insert(key, (filename, content));
        }
    }
    Ok(())
}

/// Install a Vanilla version into the exact admitted library generation.
///
/// The retained installer fetches a fresh Mojang manifest and authenticates the
/// selected version metadata itself. A catalog display record or caller-supplied
/// URL is never substituted for that authority. It retains the existing client,
/// logging, library, native, asset, and Java requirements. Launch preparation
/// materializes any legacy asset tree the index names.
///
/// Cancellation governs metadata acquisition only. Dropping the waiter alone
/// must not be reported as a cancelled installation. The caller retains `library` and
/// classify any publication, including `DownloadError::PublicationIndeterminate`,
/// before releasing target exclusion or choosing a terminal outcome.
pub async fn install<F>(
    library: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    version_id: &str,
    send: F,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<KnownGoodInstallReceipt, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    install_with_facts(library, runtime_cache, version_id, send, |_| {}, cancelled).await
}

/// Materialize Vanilla while retaining the installer's artifact evidence events.
///
/// Facts and errors are private diagnostics; the queue owns their public, bounded
/// representation. In particular, provider diagnostics must not be sent directly
/// to the UI. No terminal progress event is forwarded before settlement.
pub async fn install_with_facts<F, G>(
    library: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    version_id: &str,
    send: F,
    send_fact: G,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<KnownGoodInstallReceipt, DownloadError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    install_using_downloader(
        library,
        Downloader::new(library.clone(), runtime_cache),
        version_id,
        send,
        send_fact,
        cancelled,
    )
    .await
}

/// The acceptance seam changes provider endpoints, never manifests, verified
/// payloads, runtime authority, or publication receipts. Absent endpoints use
/// the exact same downloader construction as the production entry point.
#[cfg(feature = "test-support")]
pub(crate) async fn install_with_test_endpoints<F>(
    library: &ManagedLibraryOperation,
    runtime_cache: ManagedRuntimeCache,
    version_id: &str,
    endpoints: Option<axial_minecraft::download::InstallTestEndpoints>,
    send: F,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<KnownGoodInstallReceipt, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let downloader = Downloader::new(library.clone(), runtime_cache);
    let downloader = match endpoints {
        Some(endpoints) => downloader.with_test_endpoints(endpoints),
        None => downloader,
    };
    install_using_downloader(library, downloader, version_id, send, |_| {}, cancelled).await
}

async fn install_using_downloader<F, G>(
    library: &ManagedLibraryOperation,
    downloader: Downloader,
    version_id: &str,
    mut send: F,
    send_fact: G,
    cancelled: impl std::future::Future<Output = ()>,
) -> Result<KnownGoodInstallReceipt, DownloadError>
where
    F: FnMut(DownloadProgress),
    G: FnMut(ExecutionDownloadFact),
{
    library.revalidate().map_err(DownloadError::FileOperation)?;
    downloader
        .install_version_with_facts_cancellable(
            version_id,
            |progress| forward_pending_progress(progress, &mut send),
            send_fact,
            cancelled,
        )
        .await
}

fn forward_pending_progress<F>(progress: DownloadProgress, send: &mut F)
where
    F: FnMut(DownloadProgress),
{
    // The leaf's "done" means its publication returned a receipt. Metadata
    // activation and durable acknowledgement are still owned by the queue.
    // Likewise its error callback is not proof that concurrent effects settled.
    if !progress.done {
        send(progress);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn verified_natives_are_reused_only_while_their_exact_proof_holds() {
        use sha1::{Digest, Sha1};
        use std::io::Write;
        let root =
            tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
        let authority =
            axial_minecraft::managed_path::ManagedLibraryTestAuthority::open(root.path()).unwrap();
        let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        archive
            .start_file("fixture-native.bin", zip::write::SimpleFileOptions::default())
            .unwrap();
        archive.write_all(b"owned native fixture").unwrap();
        let archive = archive.finish().unwrap().into_inner();
        std::fs::create_dir_all(root.path().join("libraries/fixture")).unwrap();
        std::fs::write(root.path().join("libraries/fixture/native.jar"), &archive).unwrap();
        let version: VersionJson = serde_json::from_value(serde_json::json!({
            "id": "fixture", "libraries": [{
                "name": "fixture:native:1",
                "natives": {"osx":"fixture-native", "linux":"fixture-native", "windows":"fixture-native"},
                "downloads": {"classifiers":{"fixture-native":{
                    "path":"fixture/native.jar", "sha1":format!("{:x}", Sha1::digest(&archive)),
                    "size":archive.len(), "url":"https://invalid.example/native.jar"
                }}}
            }]
        }))
        .unwrap();
        let retention = NativeRetention::default();
        let prepare = || {
            let retention = retention.clone();
            let version = version.clone();
            let operation = authority.operation().clone();
            let root = root.path().to_owned();
            async move {
                prepare_natives_with_exact_files(
                    &operation,
                    &root,
                    &version,
                    &axial_minecraft::default_environment(),
                    Arc::default(),
                    Some(retention),
                )
                .await
                .unwrap()
                .unwrap()
            }
        };
        let first = prepare().await;
        let second = prepare().await;
        assert_eq!(first.path(), second.path(), "the exact inputs reuse one tree");
        assert_eq!(retention.retained(), 1);
        first.settle().unwrap();
        second.settle().unwrap();
        assert!(first.path().exists(), "an unused tree stays retained for reuse");

        // A changed extracted file fails the proof: a fresh tree replaces it
        // and the failed one is settled, not leaked.
        let tampered = first.path().join("fixture-native.bin");
        std::fs::write(&tampered, b"changed").unwrap();
        let fresh = prepare().await;
        assert_ne!(fresh.path(), first.path());
        // Changed files are preserved as an owned obligation, never reused.
        assert!(tampered.exists());
        assert_eq!(retention.retained(), 2);
        let again = prepare().await;
        assert_eq!(again.path(), fresh.path());
        again.settle().unwrap();

        // Owner release settles every unused tree it can; the changed one
        // stays owned and reported, and one still in use stays too.
        assert!(retention.settle_idle(None).is_err());
        assert!(fresh.path().exists() && tampered.exists());
        fresh.settle().unwrap();
        assert!(retention.settle_idle(None).is_err());
        assert!(!fresh.path().exists());
        assert_eq!(retention.retained(), 1);
    }
    use axial_minecraft::managed_path::ManagedLibraryTestAuthority;

    #[test]
    fn publication_and_error_events_cannot_finish_the_queue() {
        let mut observed = Vec::new();
        let downloading = DownloadProgress {
            phase: "assets".to_string(),
            current: 2,
            total: 3,
            file: Some("asset".to_string()),
            error: None,
            done: false,
            bytes_done: Some(20),
            bytes_total: Some(30),
        };
        let published = DownloadProgress {
            phase: "done".to_string(),
            done: true,
            ..downloading.clone()
        };
        let failed = DownloadProgress {
            phase: "error".to_string(),
            error: Some("private provider diagnostic".to_string()),
            done: true,
            ..downloading.clone()
        };

        for event in [downloading.clone(), published, failed] {
            forward_pending_progress(event, &mut |event| observed.push(event));
        }

        assert_eq!(observed, vec![downloading]);
    }

    #[tokio::test]
    async fn invalid_identity_has_no_payload_effect_or_terminal_progress() {
        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .expect("isolated library");
        let authority = ManagedLibraryTestAuthority::open(root.path()).expect("admitted library");
        let runtime = ManagedRuntimeCache::isolated_for_test().expect("isolated runtime");
        let initial_entries = directory_entries(root.path());

        for version_id in ["../escape", "/absolute", "", " leading", "trailing "] {
            let mut events = Vec::new();
            let mut facts = Vec::new();
            let outcome = install_with_facts(
                authority.operation(),
                runtime.clone(),
                version_id,
                |event| events.push(event),
                |fact| facts.push(fact),
                std::future::ready(()),
            )
            .await;

            assert!(matches!(outcome, Err(DownloadError::ResolveManifest(_))));
            assert!(
                events.is_empty(),
                "invalid input must not start acquisition"
            );
            assert!(facts.is_empty(), "invalid input must not fetch artifacts");
            assert_eq!(directory_entries(root.path()), initial_entries);
        }
    }

    #[tokio::test]
    async fn changed_native_source_settles_the_published_extraction_before_error() {
        let root = tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap())
            .expect("isolated library");
        let authority = ManagedLibraryTestAuthority::open(root.path()).unwrap();
        std::fs::create_dir(root.path().join("libraries")).unwrap();
        std::fs::write(
            root.path().join("libraries/native.jar"),
            b"original archive",
        )
        .unwrap();
        let relative = PortableRelativePath::new_exact("libraries/native.jar").unwrap();
        let file = authority
            .operation()
            .observe_file(&relative)
            .unwrap()
            .unwrap();
        let directory = authority
            .operation()
            .publish_native_directory(vec![
                (
                    PortableFileName::new_exact("native.bin").unwrap(),
                    b"native payload".to_vec(),
                ),
                (
                    PortableFileName::new_exact(".axial-native-manifest.json").unwrap(),
                    b"{}".to_vec(),
                ),
            ])
            .await
            .unwrap();
        let path = directory.relative_path().join_under(root.path());
        let prepared = NativeTree {
            directory,
            path: path.clone(),
            sources: vec![NativeSource {
                file,
                path: root.path().join("libraries/native.jar"),
                name: "native fixture".into(),
            }],
        };
        std::fs::write(root.path().join("libraries/native.jar"), b"changed archive").unwrap();

        assert!(matches!(
            verify_native_tree(prepared).await,
            Err(NativePreparationError::Changed)
        ));
        assert!(!path.exists());
        assert_eq!(
            std::fs::read(root.path().join("libraries/native.jar")).unwrap(),
            b"changed archive"
        );
    }

    fn directory_entries(path: &std::path::Path) -> Vec<std::ffi::OsString> {
        let mut entries = std::fs::read_dir(path)
            .expect("read isolated library")
            .map(|entry| entry.expect("library entry").file_name())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}
