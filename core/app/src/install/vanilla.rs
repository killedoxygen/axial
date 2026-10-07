//! Vanilla materialization through the retained, capability-scoped Minecraft installer.
//!
//! The returned receipt proves the published payload, not application readiness. The
//! queue must bind it to the durable publication evidence, persist its activation
//! source, and acknowledge publication before exposing a ready installation.

use axial_minecraft::download::{
    ExecutionDownloadFact, ExpectedIntegrity, LibraryVerificationIntegrity,
    library_verification_plans_for,
};
use axial_minecraft::known_good::{
    KnownGoodArtifactKind, KnownGoodIntegrity, KnownGoodInventory, KnownGoodRoot,
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

/// Exact native sources and extracted files retained until the session settles.
/// This is private capability evidence, never a serialized readiness claim.
pub(crate) struct PreparedNatives {
    directory: ManagedNativeDirectory,
    path: PathBuf,
    sources: Vec<NativeSource>,
}

impl PreparedNatives {
    /// Invoke only after the process tree and output have settled. A failed
    /// cleanup leaves this receipt owning the remaining exact files for retry.
    pub(crate) fn settle(&self) -> std::io::Result<()> {
        self.directory.settle()
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn revalidate(&self) -> Result<(), NativePreparationError> {
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
/// The path argument is accepted only as a projection of `library`.
pub(crate) async fn prepare_natives(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
) -> Result<Option<PreparedNatives>, NativePreparationError> {
    prepare_natives_with_inventory(library, library_root, version, environment, None).await
}

/// Retained install inventory supplies exact observed digests for legacy library
/// declarations that did not carry provider checksums. It must originate from
/// the acknowledged activation source for this version, not a caller DTO.
pub(crate) async fn prepare_natives_with_inventory(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    inventory: Option<Arc<KnownGoodInventory>>,
) -> Result<Option<PreparedNatives>, NativePreparationError> {
    prepare_natives_retained(library, library_root, version, environment, inventory, None).await
}

pub(crate) async fn prepare_natives_with_exact_files(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    exact: BTreeMap<String, (String, u64)>,
) -> Result<Option<PreparedNatives>, NativePreparationError> {
    prepare_natives_retained(
        library,
        library_root,
        version,
        environment,
        None,
        Some(exact),
    )
    .await
}

async fn prepare_natives_retained(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    inventory: Option<Arc<KnownGoodInventory>>,
    exact: Option<BTreeMap<String, (String, u64)>>,
) -> Result<Option<PreparedNatives>, NativePreparationError> {
    library
        .validate_read_projection(library_root)
        .map_err(|_| NativePreparationError::Changed)?;
    let root = library_root.to_owned();
    let operation = library.clone();
    let version = version.clone();
    let environment = environment.clone();
    let extraction = tokio::task::spawn_blocking(move || {
        plan_native_extraction(
            &operation,
            &root,
            &version,
            &environment,
            inventory.as_deref(),
            exact.as_ref(),
        )
    })
    .await
    .map_err(|_| NativePreparationError::Archive)??;
    let Some(extraction) = extraction else {
        return Ok(None);
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
    let prepared = PreparedNatives {
        path: directory.relative_path().join_under(library_root),
        directory,
        sources: extraction.sources,
    };
    verify_prepared_natives(prepared).await.map(Some)
}

async fn verify_prepared_natives(
    prepared: PreparedNatives,
) -> Result<PreparedNatives, NativePreparationError> {
    if let Err(error) = prepared.revalidate() {
        // A changed source does not release ownership of the extraction that
        // was already published. Accepted launch preparation keeps its task and
        // admission until this exact receipt has settled, including retries.
        settle_native_directory(prepared.directory).await;
        return Err(error);
    }
    Ok(prepared)
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

fn plan_native_extraction(
    library: &ManagedLibraryOperation,
    library_root: &Path,
    version: &VersionJson,
    environment: &Environment,
    inventory: Option<&KnownGoodInventory>,
    exact: Option<&BTreeMap<String, (String, u64)>>,
) -> Result<Option<NativeExtraction>, NativePreparationError> {
    library
        .validate_read_projection(library_root)
        .map_err(|_| NativePreparationError::Changed)?;
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
            .map_err(|_| NativePreparationError::Integrity)?;
    let verification = verification
        .into_iter()
        .map(|plan| (plan.path, plan.integrity))
        .collect::<BTreeMap<_, _>>();
    let mut sources = Vec::new();
    let mut manifest_sources = Vec::new();
    let mut extracted = BTreeMap::new();
    let mut archive_bytes = 0_u64;
    let mut expanded_bytes = 0_u64;
    let mut entry_count = 0_usize;
    for selected in selected {
        let relative = selected
            .abs_path
            .strip_prefix(library_root)
            .ok()
            .and_then(|path| PortableRelativePath::from_path(path).ok())
            .ok_or(NativePreparationError::Integrity)?;
        let expected = match verification.get(&selected.abs_path) {
            Some(LibraryVerificationIntegrity::Sha1(expected)) => expected.clone(),
            Some(LibraryVerificationIntegrity::MissingChecksum) => {
                if let Some((sha1, size)) = exact.and_then(|files| files.get(relative.as_str())) {
                    ExpectedIntegrity {
                        size: Some(*size),
                        sha1: Some(sha1.clone()),
                    }
                } else {
                    native_inventory_integrity(inventory, &relative)?
                }
            }
            None => return Err(NativePreparationError::Integrity),
        };
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

fn native_inventory_integrity(
    inventory: Option<&KnownGoodInventory>,
    relative: &PortableRelativePath,
) -> Result<ExpectedIntegrity, NativePreparationError> {
    let library_relative = relative
        .as_str()
        .strip_prefix("libraries/")
        .ok_or(NativePreparationError::Integrity)?;
    let mut matching = inventory
        .ok_or(NativePreparationError::Integrity)?
        .entries()
        .iter()
        .filter(|entry| {
            entry.root() == &KnownGoodRoot::Libraries
                && entry.path().as_str() == library_relative
                && matches!(
                    entry.kind(),
                    KnownGoodArtifactKind::Library | KnownGoodArtifactKind::NativeLibrary
                )
        });
    let entry = matching.next().ok_or(NativePreparationError::Integrity)?;
    if matching.next().is_some() {
        return Err(NativePreparationError::Integrity);
    }
    match entry.integrity() {
        KnownGoodIntegrity::Sha1 { digest, size }
        | KnownGoodIntegrity::ExactBytes { digest, size } => Ok(ExpectedIntegrity {
            size: Some(*size),
            sha1: Some(digest.as_str().to_owned()),
        }),
        _ => Err(NativePreparationError::Integrity),
    }
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
        let spelling = entry.name().replace('\\', "/");
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
/// logging, library, native, asset, legacy virtual-asset, and Java requirements.
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
        let prepared = PreparedNatives {
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
            verify_prepared_natives(prepared).await,
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
