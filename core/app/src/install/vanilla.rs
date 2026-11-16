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

/// Native directories retention owns at once, whether in use, idle, being
/// settled, unresolved, or reserved for a publication in progress.
const MAX_RETAINED_NATIVES: usize = 4;
/// Accounted bookkeeping all retained directories may hold: their source
/// paths and names, published file guards and names, and the entries.
const MAX_RETAINED_NATIVE_BYTES: u64 = 4 << 20;
/// Accounted guard state per published native file beyond its name.
const NATIVE_FILE_GUARD_BYTES: u64 = 256;

enum NativeSlot {
    /// Capacity held for a publication that has not finished.
    Reserved,
    InUse { tree: Arc<NativeTree>, users: usize },
    Idle { tree: Arc<NativeTree>, since: std::time::Instant },
    /// Its settlement is running; still counted until it reports.
    Settling,
    /// Settlement failed (its files no longer match their proof); the exact
    /// obligation stays owned here and the tree is never reused.
    Unresolved { tree: Arc<NativeTree> },
    /// Startup cleanup is retained before its first file effect and on failure.
    Recovered { directory: Arc<ManagedNativeDirectory> },
}

struct RetainedNatives {
    id: u64,
    key: [u8; 32],
    bytes: u64,
    slot: NativeSlot,
}

#[derive(Default)]
struct NativeLedger {
    next: u64,
    entries: Vec<RetainedNatives>,
}

impl NativeLedger {
    fn bytes(&self) -> u64 {
        self.entries.iter().map(|entry| entry.bytes).sum()
    }

    fn get(&mut self, id: u64) -> Option<&mut RetainedNatives> {
        self.entries.iter_mut().find(|entry| entry.id == id)
    }
}

/// Bounded reuse of verified native directories across this process's
/// sessions. Capacity for every owned directory, including one still being
/// published, settled or left unresolved, is reserved before publication; at
/// capacity an idle directory is settled first, and otherwise the session
/// owns its directory alone. An entry is reused only after its exact proof is
/// rechecked. Unused entries are settled on eviction and when the owner
/// releases them at shutdown; there is no timer. Nothing persists across a
/// restart.
#[derive(Clone, Default)]
pub(crate) struct NativeRetention {
    ledger: Arc<std::sync::Mutex<NativeLedger>>,
}

/// Capacity reserved for one fresh publication; dropping it uncommitted
/// returns the capacity.
struct NativeReservation {
    retention: NativeRetention,
    id: u64,
    committed: bool,
}

impl NativeReservation {
    fn commit(mut self, slot: NativeSlot) {
        if let Some(entry) = self.retention.lock().get(self.id) {
            entry.slot = slot;
            self.committed = true;
        }
    }
}

impl Drop for NativeReservation {
    fn drop(&mut self) {
        if !self.committed {
            let id = self.id;
            self.retention.lock().entries.retain(|entry| entry.id != id);
        }
    }
}

impl NativeRetention {
    fn lock(&self) -> std::sync::MutexGuard<'_, NativeLedger> {
        self.ledger
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A retained tree for `key`, if its proof still holds. A retained idle
    /// tree whose proof fails is settled, or kept unresolved. Blocking I/O.
    fn reuse(&self, key: &[u8; 32]) -> Option<Arc<NativeTree>> {
        let (id, candidate) = self.lock().entries.iter().find_map(|entry| match &entry.slot {
            NativeSlot::InUse { tree, .. } | NativeSlot::Idle { tree, .. } if &entry.key == key => {
                Some((entry.id, tree.clone()))
            }
            _ => None,
        })?;
        let proven = candidate.revalidate().is_ok();
        let mut ledger = self.lock();
        let entry = ledger.get(id)?;
        match &mut entry.slot {
            NativeSlot::InUse { tree, users } if proven && Arc::ptr_eq(tree, &candidate) => {
                *users += 1;
                Some(candidate)
            }
            NativeSlot::Idle { tree, .. } if Arc::ptr_eq(tree, &candidate) => {
                if proven {
                    entry.slot = NativeSlot::InUse {
                        tree: candidate.clone(),
                        users: 1,
                    };
                    return Some(candidate);
                }
                entry.slot = NativeSlot::Settling;
                drop(ledger);
                // A failed settlement stays owned as an unresolved entry.
                let _ = self.settle_entry(id, NativeSlot::Unresolved { tree: candidate });
                None
            }
            _ => None,
        }
    }

    /// Capacity for a fresh tree of `bytes` accounted size, settling the
    /// least recently used idle entries to make room. `None` when every
    /// owned directory is in use or unresolved: the caller's session then
    /// owns its directory alone. Blocking I/O.
    fn reserve(&self, key: [u8; 32], bytes: u64) -> Option<NativeReservation> {
        if bytes > MAX_RETAINED_NATIVE_BYTES {
            return None;
        }
        for _ in 0..=MAX_RETAINED_NATIVES {
            let mut ledger = self.lock();
            if ledger.entries.len() < MAX_RETAINED_NATIVES
                && ledger.bytes() + bytes <= MAX_RETAINED_NATIVE_BYTES
            {
                ledger.next = ledger.next.checked_add(1)?;
                let id = ledger.next;
                ledger.entries.push(RetainedNatives {
                    id,
                    key,
                    bytes,
                    slot: NativeSlot::Reserved,
                });
                return Some(NativeReservation {
                    retention: self.clone(),
                    id,
                    committed: false,
                });
            }
            let oldest = ledger
                .entries
                .iter_mut()
                .filter(|entry| matches!(entry.slot, NativeSlot::Idle { .. }))
                .min_by_key(|entry| match entry.slot {
                    NativeSlot::Idle { since, .. } => since,
                    _ => unreachable!("filtered to idle entries"),
                })?;
            let NativeSlot::Idle { tree, .. } =
                std::mem::replace(&mut oldest.slot, NativeSlot::Settling)
            else {
                unreachable!("filtered to idle entries");
            };
            let id = oldest.id;
            drop(ledger);
            // A failed settlement stays owned and counted as unresolved.
            let _ = self.settle_entry(id, NativeSlot::Unresolved { tree });
        }
        None
    }

    /// Settle a reserved entry, forgetting it only after all its files are gone.
    /// A failure retains the exact receipt for another owner-driven attempt.
    fn settle_entry(&self, id: u64, retained: NativeSlot) -> std::io::Result<()> {
        let result = match &retained {
            NativeSlot::Unresolved { tree } => tree.directory.settle(),
            NativeSlot::Recovered { directory } => directory.settle(),
            _ => unreachable!("only unused native directories can be settled"),
        };
        if let Err(error) = &result {
            tracing::warn!(
                io_error_kind = ?error.kind(),
                native_code = ?error.raw_os_error(),
                "Retained native directory settlement failed"
            );
        }
        let mut ledger = self.lock();
        match &result {
            Ok(()) => ledger.entries.retain(|entry| entry.id != id),
            Err(_) => {
                if let Some(entry) = ledger.get(id) {
                    entry.slot = retained;
                }
            }
        }
        result
    }

    fn recover_directory(&self, directory: ManagedNativeDirectory) -> std::io::Result<()> {
        let bytes = directory
            .retained_storage_bound()?
            .checked_add(std::mem::size_of::<RetainedNatives>() as u64)
            .ok_or_else(|| std::io::Error::other("native recovery storage exceeds capacity"))?;
        let reservation = self.reserve([0; 32], bytes).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::ResourceBusy,
                "native recovery capacity unavailable",
            )
        })?;
        let id = reservation.id;
        let directory = Arc::new(directory);
        reservation.commit(NativeSlot::Recovered {
            directory: directory.clone(),
        });
        self.settle_entry(id, NativeSlot::Recovered { directory })
    }

    /// Ends one session's use. True when the tree stays retained for reuse;
    /// false when the caller owns its settlement.
    fn release(&self, released: &Arc<NativeTree>) -> bool {
        let mut ledger = self.lock();
        let Some(entry) = ledger.entries.iter_mut().find(|entry| {
            matches!(&entry.slot, NativeSlot::InUse { tree, .. } if Arc::ptr_eq(tree, released))
        }) else {
            return false;
        };
        if let NativeSlot::InUse { tree, users } = &mut entry.slot {
            *users -= 1;
            if *users == 0 {
                entry.slot = NativeSlot::Idle {
                    tree: tree.clone(),
                    since: std::time::Instant::now(),
                };
            }
        }
        true
    }

    /// Settles every unused and unresolved entry once launches have
    /// stopped. Succeeds only when retention owns nothing afterwards; an
    /// entry in use, mid-publication or still unresolved is reported and kept.
    /// Blocking I/O.
    pub(crate) fn settle_idle(&self) -> std::io::Result<()> {
        let unused = {
            let mut ledger = self.lock();
            ledger
                .entries
                .iter_mut()
                .filter_map(|entry| {
                    let retained = match std::mem::replace(&mut entry.slot, NativeSlot::Settling) {
                        NativeSlot::Idle { tree, .. } | NativeSlot::Unresolved { tree } => {
                            NativeSlot::Unresolved { tree }
                        }
                        NativeSlot::Recovered { directory } => NativeSlot::Recovered { directory },
                        other => {
                            entry.slot = other;
                            return None;
                        }
                    };
                    Some((entry.id, retained))
                })
                .collect::<Vec<_>>()
        };
        let mut failure = None;
        for (id, retained) in unused {
            if let Err(error) = self.settle_entry(id, retained) {
                failure.get_or_insert(error);
            }
        }
        match failure {
            Some(error) => Err(error),
            None if self.lock().entries.is_empty() => Ok(()),
            None => {
                let ledger = self.lock();
                let reserved = ledger
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.slot, NativeSlot::Reserved))
                    .count();
                let in_use = ledger
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.slot, NativeSlot::InUse { .. }))
                    .count();
                let settling = ledger
                    .entries
                    .iter()
                    .filter(|entry| matches!(entry.slot, NativeSlot::Settling))
                    .count();
                tracing::warn!(
                    retained = ledger.entries.len(),
                    reserved,
                    in_use,
                    settling,
                    "Native retention still owns active directories"
                );
                Err(std::io::Error::new(
                    std::io::ErrorKind::ResourceBusy,
                    "retained native libraries are still in use",
                ))
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn retained(&self) -> usize {
        self.lock().entries.len()
    }
}

/// The bookkeeping a fresh tree from `extraction` will retain.
fn native_entry_bytes(extraction: &NativeExtraction, library_root: &Path) -> u64 {
    let sources = extraction.sources.iter().fold(0_u64, |bytes, source| {
        bytes.saturating_add(
            (source.path.as_os_str().len() + source.name.len() + std::mem::size_of::<NativeSource>())
                as u64,
        )
    });
    let files = extraction.files.iter().fold(0_u64, |bytes, (name, _)| {
        bytes.saturating_add(name.as_str().len() as u64 + NATIVE_FILE_GUARD_BYTES)
    });
    sources
        .saturating_add(files)
        .saturating_add(library_root.as_os_str().len() as u64 + 512)
        .saturating_add((std::mem::size_of::<NativeTree>() + std::mem::size_of::<RetainedNatives>()) as u64)
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

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeManifest {
    schema: u32,
    sources: Vec<NativeManifestSource>,
    files: Vec<NativeManifestFile>,
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeManifestSource {
    path: String,
    sha1: String,
    size: u64,
}

#[derive(Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct NativeManifestFile {
    name: String,
    sha1: String,
    size: u64,
}

/// Leftover native directories examined at startup.
const MAX_RECOVERED_NATIVE_DIRECTORIES: usize = 256;

/// Settles native directories an earlier run of this application published
/// and left behind (an exit while one was in use or retained). Each is
/// removed only when its own manifest proves every file it holds; anything
/// else stays exactly as found. Returns how many were settled. Blocking I/O.
pub(crate) fn recover_native_directories(
    library: &ManagedLibraryOperation,
    retention: &NativeRetention,
) -> std::io::Result<usize> {
    let listed = |bytes: &[u8]| {
        let manifest: NativeManifest = serde_json::from_slice(bytes).ok()?;
        (manifest.schema == 1)
            .then_some(manifest.files)?
            .into_iter()
            .map(|file| {
                let mut sha1 = [0; 20];
                hex::decode_to_slice(&file.sha1, &mut sha1).ok()?;
                Some((file.name, file.size, sha1))
            })
            .collect()
    };
    let mut settled = 0;
    let mut failure = None;
    for directory in library.recover_native_directories(MAX_RECOVERED_NATIVE_DIRECTORIES, listed)? {
        match retention.recover_directory(directory) {
            Ok(()) => settled += 1,
            Err(error) => {
                failure.get_or_insert(error);
            }
        }
    }
    failure.map_or(Ok(settled), Err)
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
        Fresh(Option<NativeReservation>, NativeExtraction),
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
            Some(extraction) => {
                let reservation = reuse
                    .as_ref()
                    .and_then(|retention| retention.reserve(key, native_entry_bytes(&extraction, &root)));
                Planned::Fresh(reservation, extraction)
            }
            None => Planned::None,
        })
    })
    .await
    .map_err(|_| NativePreparationError::Archive)??;
    let (reservation, extraction) = match planned {
        Planned::None => return Ok(None),
        Planned::Reused(tree) => {
            return Ok(Some(PreparedNatives {
                tree,
                retention,
                released: std::sync::atomic::AtomicBool::new(false),
            }));
        }
        Planned::Fresh(reservation, extraction) => (reservation, extraction),
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
    // Without reserved capacity this session owns its directory alone.
    let retention = reservation.map(|reservation| {
        reservation.commit(NativeSlot::InUse { tree: tree.clone(), users: 1 });
        retention.expect("a reservation comes from the retention")
    });
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

    struct NativeFixture {
        root: tempfile::TempDir,
        library: crate::library::LibraryLifecycle,
        authority: ManagedLibraryOperation,
        retention: NativeRetention,
    }

    impl NativeFixture {
        fn new() -> Self {
            let root =
                tempfile::tempdir_in(std::fs::canonicalize(std::env::temp_dir()).unwrap()).unwrap();
            let crate::library::LibraryOpenOutcome::Ready(library) =
                crate::library::LibraryLifecycle::open(root.path())
            else {
                panic!("admitted native fixture library");
            };
            let authority = library.admit().unwrap().managed_library().unwrap();
            Self {
                root,
                library,
                authority,
                retention: NativeRetention::default(),
            }
        }

        /// A version whose one native archive is distinct for each `n`.
        fn version(&self, n: usize) -> VersionJson {
            use sha1::{Digest, Sha1};
            use std::io::Write;
            let mut archive = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
            archive
                .start_file("fixture-native.bin", zip::write::SimpleFileOptions::default())
                .unwrap();
            write!(archive, "owned native fixture {n}").unwrap();
            let archive = archive.finish().unwrap().into_inner();
            let path = format!("fixture/native-{n}.jar");
            std::fs::create_dir_all(self.root.path().join("libraries/fixture")).unwrap();
            std::fs::write(self.root.path().join("libraries").join(&path), &archive).unwrap();
            serde_json::from_value(serde_json::json!({
                "id": format!("fixture-{n}"), "libraries": [{
                    "name": format!("fixture:native:{n}"),
                    "natives": {"osx":"fixture-native", "linux":"fixture-native", "windows":"fixture-native"},
                    "downloads": {"classifiers":{"fixture-native":{
                        "path": path, "sha1":format!("{:x}", Sha1::digest(&archive)),
                        "size":archive.len(), "url":"https://invalid.example/native.jar"
                    }}}
                }]
            }))
            .unwrap()
        }

        async fn prepare(&self, version: &VersionJson) -> PreparedNatives {
            prepare_natives_with_exact_files(
                &self.authority,
                self.root.path(),
                version,
                &axial_minecraft::default_environment(),
                Arc::default(),
                Some(self.retention.clone()),
            )
            .await
            .unwrap()
            .unwrap()
        }
    }

    #[tokio::test]
    async fn shutdown_settles_retained_natives_after_library_admission_closes() {
        let fixture = NativeFixture::new();
        let prepared = fixture.prepare(&fixture.version(0)).await;
        let path = prepared.path().to_owned();
        prepared.settle().unwrap();
        fixture.library.close_admission();
        fixture.retention.settle_idle().unwrap();
        assert_eq!(fixture.retention.retained(), 0);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn verified_natives_are_reused_only_while_their_exact_proof_holds() {
        let fixture = NativeFixture::new();
        let version = fixture.version(0);
        let retention = &fixture.retention;
        let first = fixture.prepare(&version).await;
        let second = fixture.prepare(&version).await;
        assert_eq!(first.path(), second.path(), "the exact inputs reuse one tree");
        assert_eq!(retention.retained(), 1);
        first.settle().unwrap();
        second.settle().unwrap();
        assert!(first.path().exists(), "an unused tree stays retained for reuse");

        // A changed extracted file fails the proof: the failed tree is kept as
        // an owned, unusable obligation and a fresh tree replaces it.
        let tampered = first.path().join("fixture-native.bin");
        std::fs::write(&tampered, b"changed").unwrap();
        let fresh = fixture.prepare(&version).await;
        assert_ne!(fresh.path(), first.path());
        assert!(tampered.exists());
        assert_eq!(retention.retained(), 2);
        let again = fixture.prepare(&version).await;
        assert_eq!(again.path(), fresh.path());
        again.settle().unwrap();

        // Owner release settles every unused tree it can and reports what
        // remains: the changed one, and one still in use.
        assert!(retention.settle_idle().is_err());
        assert!(fresh.path().exists() && tampered.exists());
        fresh.settle().unwrap();
        assert!(retention.settle_idle().is_err());
        assert!(!fresh.path().exists());
        assert!(tampered.exists());
        assert_eq!(retention.retained(), 1);
    }

    #[tokio::test]
    async fn retention_capacity_counts_every_owned_directory_before_publication() {
        let fixture = NativeFixture::new();
        let retention = &fixture.retention;
        let versions: Vec<_> = (0..7).map(|n| fixture.version(n)).collect();
        let mut held = Vec::new();
        for version in &versions[..MAX_RETAINED_NATIVES] {
            held.push(fixture.prepare(version).await);
        }
        assert_eq!(retention.retained(), MAX_RETAINED_NATIVES);

        // Every slot is in use: the next session owns its directory alone.
        let alone = fixture.prepare(&versions[4]).await;
        assert_eq!(retention.retained(), MAX_RETAINED_NATIVES);
        let alone_path = alone.path().to_owned();
        alone.settle().unwrap();
        assert!(!alone_path.exists());

        // An idle directory is settled to make room for the next publication.
        let evicted = held.remove(0);
        let evicted_path = evicted.path().to_owned();
        evicted.settle().unwrap();
        assert!(evicted_path.exists());
        let admitted = fixture.prepare(&versions[5]).await;
        assert!(!evicted_path.exists());
        assert_eq!(retention.retained(), MAX_RETAINED_NATIVES);

        // An idle directory whose settlement fails stays owned and counted.
        let damaged = held.remove(0);
        let damaged_path = damaged.path().to_owned();
        damaged.settle().unwrap();
        std::fs::write(damaged_path.join("fixture-native.bin"), b"changed").unwrap();
        let refused = fixture.prepare(&versions[6]).await;
        assert!(damaged_path.join("fixture-native.bin").exists());
        assert_eq!(retention.retained(), MAX_RETAINED_NATIVES);
        let refused_path = refused.path().to_owned();
        refused.settle().unwrap();
        assert!(!refused_path.exists(), "the session owned its directory alone");

        // A reservation still publishing blocks release, as does use.
        held.push(admitted);
        let reservation = retention.reserve([9; 32], 1);
        assert!(reservation.is_none(), "every slot is owned");
        for prepared in held {
            prepared.settle().unwrap();
        }
        let reservation = retention.reserve([9; 32], 1).unwrap();
        assert!(retention.settle_idle().is_err());
        assert!(damaged_path.exists(), "unresolved evidence is kept");
        assert_eq!(retention.retained(), 2);
        drop(reservation);
        assert!(retention.settle_idle().is_err());
        assert_eq!(retention.retained(), 1);
    }

    #[tokio::test]
    async fn failed_startup_cleanup_stays_owned_until_shutdown_can_retry() {
        let fixture = NativeFixture::new();
        let published = fixture
            .authority
            .publish_native_directory(vec![
                (
                    PortableFileName::new_exact("native.bin").unwrap(),
                    b"native".to_vec(),
                ),
                (
                    PortableFileName::new_exact(".axial-native-manifest.json").unwrap(),
                    b"{}".to_vec(),
                ),
            ])
            .await
            .unwrap();
        let path = fixture.root.path().join(published.relative_path().as_str());
        drop(published);
        let mut recovered = fixture
            .authority
            .recover_native_directories(16, |_| {
                Some(vec![(
                    "native.bin".into(),
                    6,
                    Sha1::digest(b"native").into(),
                )])
            })
            .unwrap();
        assert_eq!(recovered.len(), 1);
        std::fs::write(path.join("unlisted.bin"), b"preserved").unwrap();
        let retention = NativeRetention::default();
        assert!(
            retention
                .recover_directory(recovered.pop().unwrap())
                .is_err()
        );
        assert_eq!(retention.retained(), 1);
        assert!(retention.settle_idle().is_err());
        assert_eq!(retention.retained(), 1);
        assert_eq!(std::fs::read(path.join("native.bin")).unwrap(), b"native");
        assert_eq!(
            std::fs::read(path.join("unlisted.bin")).unwrap(),
            b"preserved"
        );
        std::fs::remove_file(path.join("unlisted.bin")).unwrap();
        retention.settle_idle().unwrap();
        assert_eq!(retention.retained(), 0);
        assert!(!path.exists());
    }

    #[tokio::test]
    async fn leftover_directories_are_settled_only_when_their_manifest_proves_them() {
        let fixture = NativeFixture::new();
        let mut paths = Vec::new();
        for n in 0..3 {
            let prepared = fixture.prepare(&fixture.version(n)).await;
            paths.push(prepared.path().to_owned());
            // An exit while retained: no owner settles these directories.
            std::mem::forget(prepared);
        }
        std::mem::forget(fixture.retention.clone());
        std::fs::write(paths[1].join("unlisted.bin"), b"user file").unwrap();
        std::fs::write(paths[2].join("fixture-native.bin"), b"changed").unwrap();
        let foreign = paths[0].parent().unwrap().join("not-a-native-directory");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::write(foreign.join("kept.bin"), b"kept").unwrap();

        let operation = fixture.authority.clone();
        let settled = tokio::task::spawn_blocking(move || {
            recover_native_directories(&operation, &NativeRetention::default())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(settled, 1);
        assert!(!paths[0].exists(), "a proven directory is settled");
        assert_eq!(
            std::fs::read(paths[1].join("unlisted.bin")).unwrap(),
            b"user file"
        );
        assert!(paths[1].join("fixture-native.bin").exists());
        assert_eq!(
            std::fs::read(paths[2].join("fixture-native.bin")).unwrap(),
            b"changed"
        );
        assert_eq!(std::fs::read(foreign.join("kept.bin")).unwrap(), b"kept");
    }

    #[tokio::test]
    async fn concurrent_reuse_and_eviction_never_exceed_capacity() {
        let fixture = NativeFixture::new();
        let versions: Vec<_> = (0..8).map(|n| fixture.version(n)).collect();
        let mut paths = BTreeSet::new();
        for round in 0..3 {
            let prepared = futures_util::future::join_all(
                versions
                    .iter()
                    .chain(&versions[..4])
                    .map(|version| fixture.prepare(version)),
            )
            .await;
            assert!(fixture.retention.retained() <= MAX_RETAINED_NATIVES, "round {round}");
            for prepared in &prepared {
                prepared.revalidate().unwrap();
                paths.insert(prepared.path().to_owned());
            }
            for prepared in prepared {
                prepared.settle().unwrap();
            }
        }
        fixture.retention.settle_idle().unwrap();
        assert_eq!(fixture.retention.retained(), 0);
        assert!(
            paths.iter().all(|path| !path.exists()),
            "every native directory was settled"
        );
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
