#[cfg(test)]
use crate::download::download_libraries_with_facts_and_descriptors;
use crate::download::{
    DownloadProgress, Downloader, ExpectedIntegrity,
    download_libraries_allowing_missing_checksums_with_facts_and_descriptors,
};
use crate::known_good::KnownGoodInstallReceipt;
use crate::launch::{DownloadEntry, VersionJson, library_merge_key, resolve_version};
use crate::loaders::compose::{
    LoaderProfileFragment, cleanup_incomplete_version, compose_loader_version,
    compose_loader_version_from_installed_base, finalize_version_install,
    prepare_managed_version_dir, write_composed_version, write_exact_managed_version_artifact,
};
use crate::loaders::forge_installer::{
    ExtractedForgeInstaller, extract_installer, extract_maven_entries,
};
#[cfg(not(test))]
use crate::loaders::http::fetch_bytes;
#[cfg(test)]
use crate::loaders::http::fetch_bytes_for_test as fetch_bytes;
use crate::loaders::processors::run_processors;
use crate::loaders::providers::{self, ProfileInstallProof};
use crate::loaders::source::{VerifiedLoaderSource, fetch_sha1_verified_source};
use crate::loaders::types::{LoaderBuildRecord, LoaderError, LoaderInstallPlan};
use crate::loaders::workspace::cleanup::{prepare_fresh_work_dir, remove_work_dir};
use crate::loaders::{
    installed_loader_metadata_bytes, validate_provider_version_id, validate_version_id,
};
use crate::paths::versions_dir;
use crate::profiles::ensure_launcher_profiles;
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;
use zip::ZipArchive;
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;
const MAX_LEGACY_OVERLAY_ENTRIES: usize = 65_536;
const MAX_LEGACY_OVERLAY_ENTRY_BYTES: u64 = 64 << 20;
const MAX_LEGACY_OVERLAY_PAYLOAD_BYTES: u64 = 256 << 20;
const MAX_LEGACY_OVERLAY_NAME_BYTES: usize = 16 << 20;
const MAX_LEGACY_OVERLAY_OVERHEAD_BYTES: usize = 16 << 20;
const MAX_LEGACY_OVERLAY_OUTPUT_BYTES: usize = 272 << 20;
const LOADER_METADATA_FILE: &str = ".axial-loader.json";

// Profile-source loaders ship a ready version JSON and then download its libraries.
pub async fn install_from_profile_source<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    profile_url: &str,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    let source_proof = providers::fetch_profile_install_proof(&plan.record).await?;
    Box::pin(install_profile_source_after_authenticated_base(
        library_dir,
        plan,
        profile_url,
        &base_receipt,
        &source_proof,
        send,
    ))
    .await
}

async fn install_profile_source_after_authenticated_base<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    profile_url: &str,
    base_receipt: &KnownGoodInstallReceipt,
    source_proof: &ProfileInstallProof,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(progress(
        "profile",
        0,
        1,
        Some("Fetching loader profile...".to_string()),
    ));
    let profile_bytes = download_to_memory(profile_url).await?;
    let mut fragment = parse_profile_json(&profile_bytes, &plan.record.component_name)?;
    validate_and_enrich_profile_source(&mut fragment, &plan.record, source_proof)?;
    let installed_version_id = plan.record.version_id.clone();
    validate_version_id(&installed_version_id, "installed loader version id")?;

    let library_download_result = Box::pin(download_profile_loader_libraries_with_evidence(
        library_dir,
        &fragment.libraries,
        "loader_libraries",
        &mut *send,
    ))
    .await;
    cleanup_on_error(library_download_result, library_dir, &installed_version_id)?;
    let version = cleanup_on_error(
        compose_loader_version(
            base_receipt.effective_version(),
            &plan.record.minecraft_version,
            &installed_version_id,
            &fragment,
        ),
        library_dir,
        &installed_version_id,
    )?;
    validate_required_profile_libraries(&version.libraries, source_proof)?;
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    let loader_metadata_bytes = installed_loader_metadata_bytes(&plan.record)?;
    let receipt = cleanup_on_error(
        KnownGoodInstallReceipt::from_verified_profile_source(
            base_receipt,
            &plan.record,
            version.clone(),
            &version_bytes,
            &fragment.libraries,
            &loader_metadata_bytes,
        )
        .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}"))),
        library_dir,
        &installed_version_id,
    )?;
    let authenticated_client = base_receipt
        .authenticated_client_integrity()
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &installed_version_id,
            &version,
            &version_bytes,
            &plan.record.minecraft_version,
            &authenticated_client,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        write_installed_loader_metadata_bytes(
            library_dir,
            &installed_version_id,
            &loader_metadata_bytes,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        ensure_launcher_profiles(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        finalize_version_install(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    send(done());
    Ok(receipt)
}

// Installer-source loaders require extracting metadata and Maven entries from the installer jar.
pub async fn install_from_installer_source<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    installer_url: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} installer...",
            plan.record.component_name
        )),
    ));
    let installer_source = fetch_sha1_verified_source(
        installer_url,
        MAX_INSTALLER_DOWNLOAD_SIZE,
        "loader installer",
    )
    .await?;
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    let authenticated_client = base_receipt
        .authenticated_client_integrity()
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    drop(base_receipt);
    let result = Box::pin(install_installer_source_after_authenticated_base(
        library_dir,
        plan,
        installer_source,
        &authenticated_client,
        send,
    ))
    .await;
    remove_work_dir(&plan.stage_dir);
    result
}

async fn install_installer_source_after_authenticated_base<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    installer_source: VerifiedLoaderSource,
    authenticated_client: &ExpectedIntegrity,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let (installer_source, extracted) =
        extract_installer_blocking(installer_source, plan.record.component_name.clone()).await?;

    send(progress(
        "profile",
        0,
        1,
        Some(format!(
            "Extracting {} installer...",
            plan.record.component_name
        )),
    ));
    if !extracted.version_id.trim().is_empty() {
        validate_provider_version_id(&extracted.version_id, "upstream installer version id")?;
    }
    let installed_version_id = plan.record.version_id.clone();
    validate_version_id(&installed_version_id, "installed loader version id")?;
    let version = compose_loader_version_from_installed_base(
        library_dir,
        &plan.record.minecraft_version,
        &installed_version_id,
        &extracted.version_fragment,
    )?;
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &installed_version_id,
            &version,
            &version_bytes,
            &plan.record.minecraft_version,
            authenticated_client,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    let installer_source = cleanup_on_error(
        extract_maven_entries_blocking(
            installer_source,
            library_dir.to_path_buf(),
            plan.record.component_name.clone(),
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    let library_download_result = Box::pin(download_profile_loader_libraries_with_evidence(
        library_dir,
        &extracted.libraries,
        "loader_libraries",
        &mut *send,
    ))
    .await;
    cleanup_on_error(library_download_result, library_dir, &installed_version_id)?;

    if let Some(install_profile_json) = extracted.install_profile_json.as_deref() {
        let stage_dir = prepare_fresh_work_dir(library_dir, &installed_version_id)?;
        if stage_dir != plan.stage_dir {
            remove_work_dir(&stage_dir);
            return Err(LoaderError::InvalidProfile(
                "installer workspace does not match its canonical install plan".to_string(),
            ));
        }
        let installer_path = plan.stage_dir.join("source-installer.jar");
        async_fs::write(&installer_path, installer_source.bytes()).await?;
        send(progress(
            "processors",
            0,
            1,
            Some("Running processors...".to_string()),
        ));
        let processor_result = Box::pin(run_processors(
            library_dir,
            &plan.record.minecraft_version,
            install_profile_json,
            installer_source.bytes(),
            &plan.stage_dir,
            &installer_path,
            |current, total, detail| {
                send(DownloadProgress {
                    phase: "processors".to_string(),
                    current: current as i32,
                    total: total as i32,
                    file: Some(detail),
                    error: None,
                    done: false,
                    bytes_done: None,
                    bytes_total: None,
                });
            },
        ))
        .await
        .map_err(|error| LoaderError::ProcessorFailed(error.to_string()));
        cleanup_on_error(processor_result, library_dir, &installed_version_id)?;
    }

    if extracted.strip_client_meta {
        send(progress(
            "client_jar",
            0,
            1,
            Some(format!("{installed_version_id}.jar")),
        ));
        cleanup_on_error(
            strip_child_client_jar_meta(library_dir, &installed_version_id).await,
            library_dir,
            &installed_version_id,
        )?;
        cleanup_on_error(
            write_patched_client_jar_integrity(library_dir, &installed_version_id).await,
            library_dir,
            &installed_version_id,
        )?;
        send(progress(
            "client_jar",
            1,
            1,
            Some(format!("{installed_version_id}.jar")),
        ));
    }

    cleanup_on_error(
        write_installed_loader_metadata(library_dir, &installed_version_id, &plan.record).await,
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        verify_install(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        ensure_launcher_profiles(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        finalize_version_install(library_dir, &installed_version_id),
        library_dir,
        &installed_version_id,
    )?;
    send(done());
    Ok(installed_version_id)
}

// Legacy archive loaders carry Maven entries in provider-specific zip layouts.
pub async fn install_from_legacy_archive<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    archive_url: &str,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} archive...",
            plan.record.component_name
        )),
    ));
    let archive_source = fetch_sha1_verified_source(
        archive_url,
        MAX_INSTALLER_DOWNLOAD_SIZE,
        "legacy Forge archive",
    )
    .await?;
    let base_receipt = Box::pin(ensure_base_version(
        library_dir,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    Box::pin(install_legacy_archive_after_authenticated_base(
        library_dir,
        plan,
        archive_source,
        &base_receipt,
        send,
    ))
    .await
}

async fn install_legacy_archive_after_authenticated_base<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    archive_source: VerifiedLoaderSource,
    base_receipt: &KnownGoodInstallReceipt,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    validate_version_id(&plan.record.version_id, "installed loader version id")?;

    let base_client_path = versions_dir(library_dir)
        .join(&plan.record.minecraft_version)
        .join(format!("{}.jar", plan.record.minecraft_version));
    let base_client_bytes = async_fs::read(&base_client_path).await?;
    base_receipt
        .authenticate_client_bytes(&base_client_bytes)
        .map_err(|error| LoaderError::Verify(format!("authenticate base client: {error:?}")))?;
    let child_client_bytes =
        overlay_legacy_archive_bytes_blocking(base_client_bytes, archive_source.into_bytes())
            .await?;

    let mut version = base_receipt.effective_version().clone();
    version.id = plan.record.version_id.clone();
    version.inherits_from = plan.record.minecraft_version.clone();
    version.materialized = true;
    let client = version.downloads.client.as_mut().ok_or_else(|| {
        LoaderError::Verify("authenticated base version has no client download".to_string())
    })?;
    client.sha1 = format!("{:x}", Sha1::digest(&child_client_bytes));
    client.size = i64::try_from(child_client_bytes.len())
        .map_err(|_| LoaderError::Verify("legacy client is too large".to_string()))?;
    client.url.clear();
    let version_bytes = serde_json::to_vec_pretty(&version)?;
    let loader_metadata_bytes = installed_loader_metadata_bytes(&plan.record)?;
    let receipt = KnownGoodInstallReceipt::from_verified_legacy_archive_source(
        base_receipt,
        &plan.record,
        version,
        &version_bytes,
        &child_client_bytes,
        &loader_metadata_bytes,
    )
    .map_err(|error| LoaderError::Verify(format!("derive loader authority: {error:?}")))?;

    let version_dir = prepare_managed_version_dir(library_dir, &plan.record.version_id)?;
    cleanup_on_error(
        write_legacy_archive_install_effects(
            &version_dir,
            &plan.record.version_id,
            &version_bytes,
            &child_client_bytes,
            &loader_metadata_bytes,
        )
        .await,
        library_dir,
        &plan.record.version_id,
    )?;

    cleanup_on_error(
        verify_install(library_dir, &plan.record.version_id),
        library_dir,
        &plan.record.version_id,
    )?;
    cleanup_on_error(
        ensure_launcher_profiles(library_dir, &plan.record.version_id),
        library_dir,
        &plan.record.version_id,
    )?;
    cleanup_on_error(
        finalize_version_install(library_dir, &plan.record.version_id),
        library_dir,
        &plan.record.version_id,
    )?;
    send(done());
    Ok(receipt)
}

async fn ensure_base_version<F>(
    library_dir: &Path,
    version_id: &str,
    send: &mut F,
) -> Result<KnownGoodInstallReceipt, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let install_lock = base_version_install_lock(library_dir, version_id);
    let _guard = install_lock.lock().await;

    let downloader = Downloader::new(library_dir.to_path_buf());
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    let result = Box::pin(downloader.install_version_with_facts_and_descriptors(
        version_id,
        |progress| {
            if !progress.done {
                send(progress);
            }
        },
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    ))
    .await;
    match result {
        Ok(receipt) => Ok(receipt),
        Err(error) => Err(LoaderError::BaseInstallFailed {
            error: Box::new(error),
            facts,
            descriptors,
        }),
    }
}

fn base_version_install_lock(library_dir: &Path, version_id: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: OnceLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mutex = LOCKS.get_or_init(|| std::sync::Mutex::new(HashMap::new()));
    base_version_install_lock_from_map(mutex, library_dir, version_id)
}

fn base_version_install_lock_from_map(
    mutex: &std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    library_dir: &Path,
    version_id: &str,
) -> Arc<tokio::sync::Mutex<()>> {
    let key = format!("{}\n{}", library_dir.to_string_lossy(), version_id.trim());
    let mut guard = match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard
        .entry(key)
        .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
        .clone()
}

#[cfg(test)]
async fn download_loader_libraries_with_evidence<F>(
    library_dir: &Path,
    libraries: &[crate::launch::Library],
    phase: &str,
    send: &mut F,
) -> Result<(), LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    download_libraries_with_facts_and_descriptors(
        library_dir,
        libraries,
        phase,
        &mut *send,
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    )
    .await
    .map_err(|_| LoaderError::ArtifactDownloadFailed { facts, descriptors })
}

async fn download_profile_loader_libraries_with_evidence<F>(
    library_dir: &Path,
    libraries: &[crate::launch::Library],
    phase: &str,
    send: &mut F,
) -> Result<(), LoaderError>
where
    F: FnMut(DownloadProgress),
{
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    download_libraries_allowing_missing_checksums_with_facts_and_descriptors(
        library_dir,
        libraries,
        phase,
        &mut *send,
        |fact| facts.push(fact),
        |descriptor| descriptors.push(descriptor),
    )
    .await
    .map_err(|_| LoaderError::ArtifactDownloadFailed { facts, descriptors })
}

fn cleanup_on_error<T, E>(
    result: Result<T, E>,
    library_dir: &Path,
    version_id: &str,
) -> Result<T, E> {
    result.inspect_err(|_| cleanup_incomplete_version(library_dir, version_id))
}

fn verify_install(library_dir: &Path, version_id: &str) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    let version = resolve_version(library_dir, version_id)
        .map_err(|error| LoaderError::Verify(format!("resolve version: {error}")))?;
    if version.main_class.trim().is_empty() {
        return Err(LoaderError::Verify("mainClass is empty".to_string()));
    }
    if version.asset_index.id.trim().is_empty() {
        return Err(LoaderError::Verify("assetIndex is empty".to_string()));
    }
    let jar_path = versions_dir(library_dir)
        .join(version_id)
        .join(format!("{version_id}.jar"));
    if !jar_path.is_file() {
        return Err(LoaderError::Verify("client jar is missing".to_string()));
    }
    Ok(())
}

async fn write_installed_loader_metadata(
    library_dir: &Path,
    version_id: &str,
    record: &LoaderBuildRecord,
) -> Result<(), LoaderError> {
    let metadata = installed_loader_metadata_bytes(record)?;
    write_installed_loader_metadata_bytes(library_dir, version_id, &metadata).await
}

async fn write_installed_loader_metadata_bytes(
    library_dir: &Path,
    version_id: &str,
    metadata: &[u8],
) -> Result<(), LoaderError> {
    let path = prepare_managed_version_dir(library_dir, version_id)?.join(LOADER_METADATA_FILE);
    write_exact_managed_version_artifact(&path, metadata).await
}

async fn download_to_memory(url: &str) -> Result<Vec<u8>, LoaderError> {
    fetch_bytes(url, MAX_INSTALLER_DOWNLOAD_SIZE).await
}

fn parse_profile_json(
    bytes: &[u8],
    component_name: &str,
) -> Result<LoaderProfileFragment, LoaderError> {
    serde_json::from_slice::<LoaderProfileFragment>(bytes)
        .map_err(|error| LoaderError::InvalidProfile(format!("{component_name} profile: {error}")))
}

fn validate_and_enrich_profile_source(
    fragment: &mut LoaderProfileFragment,
    record: &LoaderBuildRecord,
    proof: &ProfileInstallProof,
) -> Result<(), LoaderError> {
    validate_provider_version_id(&fragment.id, "upstream loader profile version id")?;
    if proof.canonical_profile_id != fragment.id
        || proof.inherits_from != fragment.inherits_from
        || fragment.inherits_from != record.minecraft_version
        || proof.client_main_class != fragment.main_class
        || fragment.main_class.trim().is_empty()
    {
        return Err(LoaderError::InvalidProfile(
            "loader profile identity does not match its live provider proof".to_string(),
        ));
    }
    if (!fragment.kind.is_empty() && fragment.kind != "release")
        || fragment.asset_index.is_some()
        || !fragment.assets.is_empty()
        || fragment.downloads.is_some()
        || fragment.java_version.is_some()
        || fragment.logging.is_some()
    {
        return Err(LoaderError::InvalidProfile(
            "loader profile overrides authenticated base-owned metadata".to_string(),
        ));
    }

    if proof.required_libraries.is_empty() {
        return Err(LoaderError::InvalidProfile(
            "loader profile libraries do not match their live provider proof".to_string(),
        ));
    }
    validate_required_profile_libraries(&fragment.libraries, proof)?;
    for required in &proof.required_libraries {
        let library = fragment
            .libraries
            .iter_mut()
            .find(|library| library.name == required.coordinate)
            .expect("validated required profile library");
        enrich_library_integrity(library, required)?;
    }
    Ok(())
}

fn validate_required_profile_libraries(
    libraries: &[crate::launch::Library],
    proof: &ProfileInstallProof,
) -> Result<(), LoaderError> {
    for required in &proof.required_libraries {
        let required_key = library_merge_key(&required.coordinate);
        let matching_key = libraries
            .iter()
            .filter(|library| library_merge_key(&library.name) == required_key)
            .collect::<Vec<_>>();
        if matching_key.len() != 1 || matching_key[0].name != required.coordinate {
            return Err(LoaderError::InvalidProfile(
                "loader profile contains a missing, duplicate, or shadowed provider library"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn enrich_library_integrity(
    library: &mut crate::launch::Library,
    proof: &crate::loaders::providers::ProfileLibraryProof,
) -> Result<(), LoaderError> {
    let (Some(expected_sha1), Some(expected_size)) = (proof.sha1.as_deref(), proof.size) else {
        return Ok(());
    };
    let expected_size = i64::try_from(expected_size).map_err(|_| {
        LoaderError::InvalidProfile("provider library size is out of range".to_string())
    })?;
    if (!library.sha1.is_empty() && !library.sha1.eq_ignore_ascii_case(expected_sha1))
        || (library.size > 0 && library.size != expected_size)
    {
        return Err(LoaderError::InvalidProfile(
            "loader profile library integrity conflicts with its live provider proof".to_string(),
        ));
    }
    library.sha1 = expected_sha1.to_string();
    library.size = expected_size;
    if let Some(artifact) = library
        .downloads
        .as_mut()
        .and_then(|downloads| downloads.artifact.as_mut())
    {
        if (!artifact.sha1.is_empty() && !artifact.sha1.eq_ignore_ascii_case(expected_sha1))
            || (artifact.size > 0 && artifact.size != expected_size)
        {
            return Err(LoaderError::InvalidProfile(
                "loader profile artifact integrity conflicts with its live provider proof"
                    .to_string(),
            ));
        }
        artifact.sha1 = expected_sha1.to_string();
        artifact.size = expected_size;
    }
    Ok(())
}

async fn extract_installer_blocking(
    installer_source: VerifiedLoaderSource,
    component_name: String,
) -> Result<(VerifiedLoaderSource, ExtractedForgeInstaller), LoaderError> {
    tokio::task::spawn_blocking(move || {
        let extracted = extract_installer(installer_source.bytes())
            .map_err(|error| installer_extract_error(&component_name, error))?;
        Ok((installer_source, extracted))
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn extract_maven_entries_blocking(
    installer_source: VerifiedLoaderSource,
    library_dir: PathBuf,
    component_name: String,
) -> Result<VerifiedLoaderSource, LoaderError> {
    tokio::task::spawn_blocking(move || {
        extract_maven_entries(installer_source.bytes(), &library_dir)
            .map_err(|error| installer_extract_error(&component_name, error))?;
        Ok(installer_source)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn overlay_legacy_archive_bytes_blocking(
    base_client_bytes: Vec<u8>,
    archive_data: Vec<u8>,
) -> Result<Vec<u8>, LoaderError> {
    tokio::task::spawn_blocking(move || {
        overlay_legacy_archive_bytes(&base_client_bytes, &archive_data)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn write_legacy_archive_install_effects(
    version_dir: &Path,
    version_id: &str,
    version_bytes: &[u8],
    child_client_bytes: &[u8],
    loader_metadata_bytes: &[u8],
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    write_exact_managed_version_artifact(&version_dir.join(".incomplete"), b"installing").await?;
    write_exact_managed_version_artifact(
        &version_dir.join(format!("{version_id}.json")),
        version_bytes,
    )
    .await?;
    write_exact_managed_version_artifact(
        &version_dir.join(format!("{version_id}.jar")),
        child_client_bytes,
    )
    .await?;
    write_exact_managed_version_artifact(
        &version_dir.join(LOADER_METADATA_FILE),
        loader_metadata_bytes,
    )
    .await
}

async fn write_patched_client_jar_integrity(
    library_dir: &Path,
    version_id: &str,
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    let version_dir = versions_dir(library_dir).join(version_id);
    let version_path = version_dir.join(format!("{version_id}.json"));
    let jar_path = version_dir.join(format!("{version_id}.jar"));
    let jar_bytes = async_fs::read(&jar_path).await?;
    let mut hasher = Sha1::new();
    hasher.update(&jar_bytes);
    let sha1 = format!("{:x}", hasher.finalize());
    let size = i64::try_from(jar_bytes.len()).unwrap_or(i64::MAX);

    let version_bytes = async_fs::read(&version_path).await?;
    let mut version: VersionJson = serde_json::from_slice(&version_bytes).map_err(|error| {
        LoaderError::InvalidProfile(format!("parse installed version: {error}"))
    })?;
    let client = version
        .downloads
        .client
        .get_or_insert_with(DownloadEntry::default);
    client.sha1 = sha1;
    client.size = size;
    client.url.clear();
    async_fs::write(&version_path, serde_json::to_vec_pretty(&version)?).await?;
    Ok(())
}

async fn strip_child_client_jar_meta(
    library_dir: &Path,
    version_id: &str,
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    let jar_path = versions_dir(library_dir)
        .join(version_id)
        .join(format!("{version_id}.jar"));
    let temp_jar = artifact_tmp_path(&jar_path);
    let source_jar = jar_path.clone();
    let blocking_temp_jar = temp_jar.clone();
    tokio::task::spawn_blocking(move || {
        strip_zip_metadata_blocking(&source_jar, &blocking_temp_jar)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))??;

    match async_fs::remove_file(&jar_path).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            let _ = async_fs::remove_file(&temp_jar).await;
            return Err(LoaderError::Io(error));
        }
    }
    match async_fs::rename(&temp_jar, &jar_path).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = async_fs::remove_file(&temp_jar).await;
            Err(LoaderError::Io(error))
        }
    }
}

fn strip_zip_metadata_blocking(source_jar: &Path, temp_jar: &Path) -> Result<(), LoaderError> {
    if let Some(parent) = temp_jar.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let source_file = File::open(source_jar)?;
    let mut source_archive = ZipArchive::new(source_file)
        .map_err(|error| legacy_archive_error("legacy client", error))?;
    let output_file = File::create(temp_jar)?;
    let mut writer = ZipWriter::new(output_file);
    copy_zip_entries(&mut source_archive, &mut writer, None)?;
    writer
        .finish()
        .map_err(|error| legacy_archive_error("legacy client metadata strip", error))?;
    Ok(())
}

fn overlay_legacy_archive_bytes(
    base_client_bytes: &[u8],
    archive_data: &[u8],
) -> Result<Vec<u8>, LoaderError> {
    let mut base_archive = ZipArchive::new(std::io::Cursor::new(base_client_bytes))
        .map_err(|error| legacy_archive_error("base Minecraft", error))?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    let forge_names = legacy_overlay_entry_names(&mut forge_archive)?;
    let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
    let mut budget = LegacyOverlayBudget::default();

    copy_legacy_overlay_entries(
        &mut base_archive,
        &mut writer,
        Some(&forge_names),
        &mut budget,
    )?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    copy_legacy_overlay_entries(&mut forge_archive, &mut writer, None, &mut budget)?;
    let output = writer
        .finish()
        .map(|cursor| cursor.into_inner())
        .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
    if output.len() > MAX_LEGACY_OVERLAY_OUTPUT_BYTES {
        return Err(legacy_overlay_limit_error());
    }
    Ok(output)
}

fn legacy_overlay_entry_names<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<HashSet<String>, LoaderError> {
    let mut names = HashSet::new();
    let mut name_bytes = 0usize;
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("Forge", error))?;
        if index >= MAX_LEGACY_OVERLAY_ENTRIES {
            return Err(legacy_overlay_limit_error());
        }
        name_bytes = name_bytes
            .checked_add(entry.name().len())
            .ok_or_else(legacy_overlay_limit_error)?;
        if name_bytes > MAX_LEGACY_OVERLAY_NAME_BYTES {
            return Err(legacy_overlay_limit_error());
        }
        if legacy_archive_entry_is_skipped(entry.name()) {
            continue;
        }
        names.insert(entry.name().to_string());
    }
    Ok(names)
}

#[derive(Default)]
struct LegacyOverlayBudget {
    entries: usize,
    payload_bytes: u64,
    name_bytes: usize,
    output_overhead_bytes: usize,
}

impl LegacyOverlayBudget {
    fn reserve(&mut self, name: &str, size: u64) -> Result<(), LoaderError> {
        if size > MAX_LEGACY_OVERLAY_ENTRY_BYTES {
            return Err(legacy_overlay_limit_error());
        }
        self.entries = self
            .entries
            .checked_add(1)
            .ok_or_else(legacy_overlay_limit_error)?;
        self.payload_bytes = self
            .payload_bytes
            .checked_add(size)
            .ok_or_else(legacy_overlay_limit_error)?;
        self.name_bytes = self
            .name_bytes
            .checked_add(name.len())
            .ok_or_else(legacy_overlay_limit_error)?;
        self.output_overhead_bytes = self
            .output_overhead_bytes
            .checked_add(
                name.len()
                    .checked_mul(2)
                    .and_then(|bytes| bytes.checked_add(256))
                    .ok_or_else(legacy_overlay_limit_error)?,
            )
            .ok_or_else(legacy_overlay_limit_error)?;
        if self.entries > MAX_LEGACY_OVERLAY_ENTRIES
            || self.payload_bytes > MAX_LEGACY_OVERLAY_PAYLOAD_BYTES
            || self.name_bytes > MAX_LEGACY_OVERLAY_NAME_BYTES
            || self.output_overhead_bytes > MAX_LEGACY_OVERLAY_OVERHEAD_BYTES
        {
            return Err(legacy_overlay_limit_error());
        }
        Ok(())
    }
}

fn copy_legacy_overlay_entries<
    R: std::io::Read + std::io::Seek,
    W: std::io::Write + std::io::Seek,
>(
    archive: &mut ZipArchive<R>,
    writer: &mut ZipWriter<W>,
    replaced_names: Option<&HashSet<String>>,
    budget: &mut LegacyOverlayBudget,
) -> Result<(), LoaderError> {
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("legacy Forge", error))?;
        let name = entry.name().to_string();
        if legacy_archive_entry_is_skipped(&name)
            || replaced_names.is_some_and(|names| names.contains(&name))
        {
            continue;
        }
        budget.reserve(&name, entry.size())?;
        if entry.is_dir() || name.ends_with('/') {
            writer
                .add_directory(&name, SimpleFileOptions::default())
                .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
            continue;
        }

        let expected_size = entry.size();
        let capacity = usize::try_from(expected_size).map_err(|_| legacy_overlay_limit_error())?;
        let mut bytes = Vec::with_capacity(capacity);
        entry
            .by_ref()
            .take(expected_size.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(LoaderError::Io)?;
        if bytes.len() as u64 != expected_size {
            return Err(legacy_overlay_limit_error());
        }
        writer
            .start_file(&name, SimpleFileOptions::default())
            .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
        writer.write_all(&bytes).map_err(LoaderError::Io)?;
    }
    Ok(())
}

fn legacy_overlay_limit_error() -> LoaderError {
    LoaderError::InvalidProfile("legacy Forge overlay exceeds bounded output limits".to_string())
}

fn copy_zip_entries<R: std::io::Read + std::io::Seek, W: std::io::Write + std::io::Seek>(
    archive: &mut ZipArchive<R>,
    writer: &mut ZipWriter<W>,
    replaced_names: Option<&HashSet<String>>,
) -> Result<(), LoaderError> {
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("legacy Forge", error))?;
        let name = entry.name().to_string();
        if legacy_archive_entry_is_skipped(&name)
            || replaced_names.is_some_and(|names| names.contains(&name))
        {
            continue;
        }
        if entry.is_dir() || name.ends_with('/') {
            writer
                .add_directory(&name, SimpleFileOptions::default())
                .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
            continue;
        }

        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).map_err(LoaderError::Io)?;
        writer
            .start_file(&name, SimpleFileOptions::default())
            .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
        writer.write_all(&bytes).map_err(LoaderError::Io)?;
    }
    Ok(())
}

fn legacy_archive_entry_is_skipped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    upper == "META-INF/MANIFEST.MF"
        || upper.ends_with(".SF")
        || upper.ends_with(".RSA")
        || upper.ends_with(".DSA")
}

fn artifact_tmp_path(path: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    path.with_extension(format!("tmp-{}-{nanos:x}", std::process::id()))
}

fn installer_extract_error(component_name: &str, error: impl std::fmt::Display) -> LoaderError {
    LoaderError::InvalidProfile(format!("extracting {component_name} installer: {error}"))
}

fn legacy_archive_error(component_name: &str, error: impl std::fmt::Display) -> LoaderError {
    LoaderError::InvalidProfile(format!(
        "validating {component_name} legacy archive: {error}"
    ))
}

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
        bytes_done: None,
        bytes_total: None,
    }
}

fn done() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        base_version_install_lock_from_map, cleanup_on_error,
        download_loader_libraries_with_evidence, download_profile_loader_libraries_with_evidence,
        ensure_base_version, fetch_sha1_verified_source, install_from_installer_source,
        install_from_legacy_archive, install_from_profile_source,
        install_installer_source_after_authenticated_base,
        install_legacy_archive_after_authenticated_base, overlay_legacy_archive_bytes,
        strip_child_client_jar_meta, validate_and_enrich_profile_source,
        write_patched_client_jar_integrity,
    };
    use crate::download::{
        DownloadProgress, ExecutionDownloadFactKind, ExpectedIntegrity,
        SelectedDownloadArtifactKind,
    };
    use crate::known_good::{KnownGoodArtifactKind, KnownGoodInstallReceipt, KnownGoodIntegrity};
    use crate::launch::{
        AssetIndex, Downloads, JavaVersion, Library, LibraryArtifact, LibraryDownload, LoggingConf,
        resolve_version,
    };
    use crate::loaders::compose::LoaderProfileFragment;
    use crate::loaders::providers::{ProfileInstallProof, ProfileLibraryProof};
    use crate::loaders::source::VerifiedLoaderSource;
    use crate::loaders::types::LoaderError;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability,
    };
    use crate::loaders::{build_id_for, installed_version_id_for, validate_version_id};
    use crate::paths::versions_dir;
    use crate::rules::default_environment;
    use sha1::{Digest as _, Sha1};
    use std::collections::HashMap;
    use std::fs;
    use std::io::{ErrorKind, Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cleanup_on_error_removes_incomplete_version_dir() {
        let root = temp_dir("cleanup-on-error");
        let version_dir = versions_dir(&root).join("broken-loader");
        fs::create_dir_all(&version_dir).expect("version dir");
        fs::write(version_dir.join(".incomplete"), b"installing").expect("marker");

        let result = cleanup_on_error::<(), _>(
            Err(LoaderError::Verify("broken".to_string())),
            &root,
            "broken-loader",
        );

        assert!(result.is_err());
        assert!(!version_dir.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn profile_source_requires_exact_live_identity_and_unique_coordinates() {
        let record = profile_record();
        let proof = fabric_profile_proof(&record);
        let mut fragment = fabric_profile_fragment(&record);

        validate_and_enrich_profile_source(&mut fragment, &record, &proof)
            .expect("exact live profile proof");
        let loader = fragment
            .libraries
            .iter()
            .find(|library| library.name.starts_with("net.fabricmc:fabric-loader:"))
            .expect("loader library");
        assert_eq!(loader.sha1, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        assert_eq!(loader.size, 42);

        fragment.libraries.push(Library {
            name: "net.fabricmc:fabric-loader:competing".to_string(),
            ..Library::default()
        });
        assert!(validate_and_enrich_profile_source(&mut fragment, &record, &proof).is_err());
    }

    #[test]
    fn profile_source_rejects_identity_drift_and_base_owned_overrides() {
        let record = profile_record();
        let proof = fabric_profile_proof(&record);

        let mut variants = Vec::new();
        let mut fragment = fabric_profile_fragment(&record);
        fragment.id.push_str("-wrong");
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.inherits_from = "1.21.4".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.main_class = "wrong.Main".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.kind = "snapshot".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.asset_index = Some(AssetIndex::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.assets = "legacy".to_string();
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.downloads = Some(Downloads::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.java_version = Some(JavaVersion::default());
        variants.push(fragment);
        let mut fragment = fabric_profile_fragment(&record);
        fragment.logging = Some(LoggingConf::default());
        variants.push(fragment);

        for mut fragment in variants {
            assert!(
                validate_and_enrich_profile_source(&mut fragment, &record, &proof).is_err(),
                "identity drift or base-owned override must fail"
            );
        }
    }

    #[test]
    fn loader_install_futures_stay_small_enough_for_tokio_workers() {
        let root = PathBuf::from("/tmp/axial-loader-future-size");
        let profile_plan = LoaderInstallPlan {
            record: profile_record(),
            stage_dir: root.join("profile-stage"),
        };
        let installer_plan = LoaderInstallPlan {
            record: installer_record(),
            stage_dir: root.join("installer-stage"),
        };
        let legacy_plan = LoaderInstallPlan {
            record: legacy_archive_record(),
            stage_dir: root.join("legacy-stage"),
        };

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&ensure_base_version(&root, "1.21.5", &mut send)) < 4096,
            "loader base-version future should not embed the full vanilla install future"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_profile_source(
                &root,
                &profile_plan,
                "https://example.test/profile.json",
                &mut send,
            )) < 4096,
            "profile-backed loader install future should stay small"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_installer_source(
                &root,
                &installer_plan,
                "https://example.test/installer.jar",
                &mut send,
            )) < 4096,
            "installer-backed loader install future should stay small"
        );

        let mut send = |_progress: DownloadProgress| {};
        assert!(
            std::mem::size_of_val(&install_from_legacy_archive(
                &root,
                &legacy_plan,
                "https://example.test/legacy.jar",
                &mut send,
            )) < 4096,
            "legacy archive loader install future should stay small"
        );

        assert!(
            std::mem::size_of_val(&super::super::install_build(&root, &installer_plan, |_| {}))
                < 4096,
            "loader strategy dispatcher future should not embed the largest strategy branch"
        );

        assert!(
            std::mem::size_of_val(&crate::loaders::install_build(
                &root,
                installer_plan.record.clone(),
                |_| {}
            )) < 4096,
            "public loader install future should not embed the strategy dispatcher"
        );
    }

    #[tokio::test]
    async fn base_version_install_lock_serializes_same_library_version() {
        let locks = std::sync::Mutex::new(HashMap::new());
        let root = PathBuf::from("/tmp/axial-loader-base-lock");
        let first = base_version_install_lock_from_map(&locks, &root, "1.21.5");
        let second = base_version_install_lock_from_map(&locks, &root, "1.21.5");
        let other_version = base_version_install_lock_from_map(&locks, &root, "1.21.4");
        let other_library =
            base_version_install_lock_from_map(&locks, &root.join("other"), "1.21.5");

        assert!(Arc::ptr_eq(&first, &second));
        assert!(!Arc::ptr_eq(&first, &other_version));
        assert!(!Arc::ptr_eq(&first, &other_library));

        let first_guard = first.lock().await;
        let second_wait = tokio::spawn(async move {
            let _guard = second.lock().await;
        });
        tokio::task::yield_now().await;
        assert!(!second_wait.is_finished());

        drop(first_guard);
        tokio::time::timeout(Duration::from_secs(1), second_wait)
            .await
            .expect("second waiter should acquire after first guard drops")
            .expect("second waiter should not panic");
    }

    #[test]
    fn base_version_install_lock_recovers_from_poisoned_map_lock() {
        let locks = Arc::new(std::sync::Mutex::new(HashMap::new()));
        let root = PathBuf::from("/tmp/axial-loader-poisoned-base-lock");
        let seeded_install_lock = Arc::new(tokio::sync::Mutex::new(()));
        let poison_target = Arc::clone(&locks);
        let poison_seed = Arc::clone(&seeded_install_lock);
        let poison_root = root.clone();

        let _ = std::thread::spawn(move || {
            let key = format!("{}\n{}", poison_root.to_string_lossy(), "1.21.5");
            let mut guard = poison_target.lock().unwrap();
            guard.insert(key, poison_seed);
            panic!("poison base install lock map");
        })
        .join();

        assert!(locks.is_poisoned());
        let recovered_lock = base_version_install_lock_from_map(&locks, &root, "1.21.5");

        assert!(Arc::ptr_eq(&recovered_lock, &seeded_install_lock));
    }

    #[tokio::test]
    async fn loader_library_download_failure_carries_artifact_evidence() {
        let root = temp_dir("loader-library-evidence");
        let server = TestByteServer::start(b"wrong".to_vec());
        let expected = b"fresh";
        let library = Library {
            name: "com.example:loader-lib:1.0.0".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: "com/example/loader-lib/1.0.0/loader-lib-1.0.0.jar".to_string(),
                    sha1: sha1_hex(expected),
                    size: expected.len() as i64,
                    url: server.url.clone(),
                }),
                ..LibraryDownload::default()
            }),
            ..Library::default()
        };

        let error = download_loader_libraries_with_evidence(
            &root,
            &[library],
            "loader_libraries",
            &mut |_progress| {},
        )
        .await
        .expect_err("checksum mismatch should carry evidence");

        match error {
            LoaderError::ArtifactDownloadFailed { facts, descriptors } => {
                assert!(
                    facts
                        .iter()
                        .any(|fact| fact.kind == ExecutionDownloadFactKind::ArtifactMissing)
                );
                assert!(
                    facts
                        .iter()
                        .any(|fact| fact.kind == ExecutionDownloadFactKind::ChecksumMismatch)
                );
                assert!(descriptors.iter().any(|descriptor| {
                    descriptor.kind == SelectedDownloadArtifactKind::Library
                        && descriptor.target == "minecraft_library_loader-lib-1.0.0"
                }));
            }
            other => panic!("expected artifact download evidence, got {other:?}"),
        }

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn profile_loader_library_download_allows_missing_checksum_metadata() {
        let root = temp_dir("profile-loader-library-missing-checksum");
        let body = zip_entries(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]);
        let server = TestByteServer::start(body.clone());
        let artifact_path = "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar";
        let library = Library {
            name: "org.quiltmc:quilt-loader:0.29.2".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: artifact_path.to_string(),
                    url: server.url.clone(),
                    ..LibraryArtifact::default()
                }),
                ..LibraryDownload::default()
            }),
            ..Library::default()
        };

        download_profile_loader_libraries_with_evidence(
            &root,
            &[library],
            "loader_libraries",
            &mut |_progress| {},
        )
        .await
        .expect("profile loader library should allow missing checksum metadata");

        assert_eq!(
            fs::read(root.join("libraries").join(artifact_path)).expect("read loader library"),
            body
        );
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn profile_loader_library_download_replaces_invalid_checksumless_jar() {
        let root = temp_dir("profile-loader-library-invalid-checksumless-jar");
        let body = zip_entries(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]);
        let server = TestByteServer::start(body.clone());
        let artifact_path = "org/quiltmc/quilt-loader/0.29.2/quilt-loader-0.29.2.jar";
        let destination = root.join("libraries").join(artifact_path);
        fs::create_dir_all(destination.parent().expect("artifact parent"))
            .expect("create artifact parent");
        fs::write(&destination, b"not a jar").expect("write invalid cached jar");
        let library = Library {
            name: "org.quiltmc:quilt-loader:0.29.2".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: artifact_path.to_string(),
                    url: server.url.clone(),
                    ..LibraryArtifact::default()
                }),
                ..LibraryDownload::default()
            }),
            ..Library::default()
        };

        download_profile_loader_libraries_with_evidence(
            &root,
            &[library],
            "loader_libraries",
            &mut |_progress| {},
        )
        .await
        .expect("invalid checksumless loader jar should be replaced");

        assert_eq!(fs::read(&destination).expect("read loader library"), body);
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_whitespace_only_installed_version_id() {
        let error = validate_version_id(" \n ", "installed loader version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "installed loader version id is empty"
        ));
    }

    #[test]
    fn rejects_whitespace_padded_installed_version_id() {
        let error =
            validate_version_id(" loader-id ", "installed loader version id").expect_err("error");
        assert!(matches!(
            error,
            LoaderError::InstallExecutionFailed(message)
                if message == "installed loader version id contains surrounding whitespace"
        ));
    }

    #[tokio::test]
    async fn every_installer_strategy_rejects_sha1_mismatch_before_base_effects() {
        for (component, strategy) in [
            (
                LoaderComponentId::Forge,
                LoaderInstallStrategy::ForgeLegacyInstaller,
            ),
            (LoaderComponentId::Forge, LoaderInstallStrategy::ForgeModern),
            (
                LoaderComponentId::NeoForge,
                LoaderInstallStrategy::NeoForgeModern,
            ),
        ] {
            let root = temp_dir("installer-source-sha1-mismatch");
            let server = TestByteServer::start_with_sha1_proof(
                installer_jar("upstream-installer-id"),
                vec![b'0'; 40],
            );
            let mut record = installer_record();
            record.component_id = component;
            record.component_name = component.display_name().to_string();
            record.strategy = strategy;
            canonicalize_record_identity(&mut record);
            let plan = LoaderInstallPlan {
                record,
                stage_dir: root.join("stage"),
            };

            let error = install_from_installer_source(&root, &plan, &server.url, &mut |_| {})
                .await
                .expect_err("mismatched proof must fail");

            assert!(
                matches!(error, LoaderError::Verify(message) if message.contains("live sha1 proof"))
            );
            assert!(!root.exists());
            assert_eq!(server.request_count(), 2);
            server.stop();
        }
    }

    #[tokio::test]
    async fn installer_source_rejects_malformed_sha1_proof_before_base_effects() {
        let root = temp_dir("installer-source-sha1-malformed");
        let server = TestByteServer::start_with_sha1_proof(
            installer_jar("upstream-installer-id"),
            b"not-a-digest installer.jar".to_vec(),
        );
        let record = installer_record();
        let plan = LoaderInstallPlan {
            record,
            stage_dir: root.join("stage"),
        };

        let error = install_from_installer_source(&root, &plan, &server.url, &mut |_| {})
            .await
            .expect_err("malformed proof must fail");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("exactly one 40-hex digest"))
        );
        assert!(!root.exists());
        assert_eq!(server.request_count(), 2);
        server.stop();
    }

    #[tokio::test]
    async fn installer_source_installs_to_backend_version_id_when_upstream_id_differs() {
        let root = temp_dir("installer-upstream-id-mismatch");
        write_base_version(&root, "1.21.5");
        let record = installer_record();
        let installer_server =
            TestByteServer::start_with_sha1(installer_jar("upstream-installer-id"));
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let mut progress = |_progress: DownloadProgress| {};
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;

        let installed_version_id = install_installer_source_after_authenticated_base(
            &root,
            &plan,
            installer_source,
            &test_client_integrity(&root, &record.minecraft_version),
            &mut progress,
        )
        .await
        .expect("install installer-backed loader");

        assert_eq!(installed_version_id, record.version_id);
        assert_backend_version_was_written(&root, &record.version_id, "upstream-installer-id");
        assert_eq!(installer_server.request_count(), 2);
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installer_source_allows_checksumless_legacy_profile_libraries() {
        let root = temp_dir("installer-checksumless-legacy-libraries");
        let minecraft_version = "1.7.10";
        write_base_version(&root, minecraft_version);
        let library_body = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
        let library_server = TestByteServer::start(library_body);
        let mut record = installer_record();
        record.minecraft_version = minecraft_version.to_string();
        record.loader_version = "10.13.4.1614-1.7.10".to_string();
        canonicalize_record_identity(&mut record);
        let installer_server = TestByteServer::start_with_sha1(installer_jar_with_profile_json(
                format!(
                    r#"{{
                        "id":"upstream-forge-1.7.10",
                        "inheritsFrom":"{minecraft_version}",
                        "mainClass":"net.minecraft.launchwrapper.Launch",
                        "minecraftArguments":"--username ${{auth_player_name}} --accessToken ${{auth_access_token}}",
                        "libraries":[{{
                            "name":"net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10",
                            "url":"{}"
                        }}]
                    }}"#,
                    library_server.url
                )
                .as_bytes(),
            ));
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;

        let installed_version_id = install_installer_source_after_authenticated_base(
            &root,
            &plan,
            installer_source,
            &test_client_integrity(&root, &record.minecraft_version),
            &mut |_| {},
        )
        .await
        .expect("install legacy installer-backed loader");

        assert_eq!(installed_version_id, record.version_id);
        assert_eq!(installer_server.request_count(), 2);
        assert_eq!(library_server.request_count(), 1);
        let library_path = root.join("libraries").join(
            "net/minecraftforge/forge/1.7.10-10.13.4.1614-1.7.10/forge-1.7.10-10.13.4.1614-1.7.10.jar",
        );
        assert!(zip_contains(
            &library_path,
            "net/minecraftforge/Forge.class"
        ));
        let version_json = fs::read(
            versions_dir(&root)
                .join(&record.version_id)
                .join(format!("{}.json", record.version_id)),
        )
        .expect("read installer profile");
        let version: serde_json::Value =
            serde_json::from_slice(&version_json).expect("parse installer profile");
        let libraries = version["libraries"].as_array().expect("libraries");
        assert!(libraries.iter().any(|library| {
            library["name"] == "net.minecraftforge:forge:1.7.10-10.13.4.1614-1.7.10"
                && library.get("axialChecksumlessAllowed").is_none()
        }));

        installer_server.stop();
        library_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn strip_meta_legacy_installer_rewrites_child_client_and_integrity() {
        let root = temp_dir("installer-strip-meta-child-client");
        let version_id = "patched-child";
        let version_dir = versions_dir(&root).join(version_id);
        fs::create_dir_all(&version_dir).expect("version dir");
        let signed_client = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/MOJANG_C.SF", b"signature".as_slice()),
            ("META-INF/MOJANG_C.RSA", b"signature".as_slice()),
            ("net/minecraft/client/Minecraft.class", b"class".as_slice()),
        ]);
        fs::write(
            version_dir.join(format!("{version_id}.jar")),
            &signed_client,
        )
        .expect("write signed child client");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            r#"{
                "id":"patched-child",
                "type":"release",
                "mainClass":"net.minecraft.launchwrapper.Launch",
                "assetIndex":{"id":"pre-1.6","url":"","sha1":"","size":0,"totalSize":0},
                "downloads":{
                    "client":{
                        "url":"https://example.invalid/1.5.2.jar",
                        "sha1":"originalvanillasha1",
                        "size":123
                    }
                },
                "libraries":[]
            }"#,
        )
        .expect("write version json");

        strip_child_client_jar_meta(&root, version_id)
            .await
            .expect("strip child client metadata");
        write_patched_client_jar_integrity(&root, version_id)
            .await
            .expect("write stripped client integrity");

        let installed_jar = version_dir.join(format!("{version_id}.jar"));
        assert!(zip_contains(
            &installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&installed_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&installed_jar, "META-INF/MOJANG_C.SF"));
        assert!(!zip_contains(&installed_jar, "META-INF/MOJANG_C.RSA"));
        let installed_jar_bytes = fs::read(&installed_jar).expect("read stripped jar");
        let installed_version_json =
            fs::read_to_string(version_dir.join(format!("{version_id}.json")))
                .expect("read version json");
        let installed_version: serde_json::Value =
            serde_json::from_str(&installed_version_json).expect("parse version json");
        assert_eq!(
            installed_version["downloads"]["client"]["sha1"],
            sha1_hex(&installed_jar_bytes)
        );
        assert_eq!(
            installed_version["downloads"]["client"]["size"],
            installed_jar_bytes.len() as i64
        );
        assert_eq!(installed_version["downloads"]["client"]["url"], "");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn strip_meta_legacy_installer_install_strips_child_not_base_client() {
        let root = temp_dir("installer-strip-meta-install");
        let minecraft_version = "1.5.2";
        let loader_version = "7.8.1.738";
        let version_id =
            installed_version_id_for(LoaderComponentId::Forge, minecraft_version, loader_version)
                .expect("canonical installed version id");
        let base_dir = versions_dir(&root).join(minecraft_version);
        fs::create_dir_all(&base_dir).expect("base version dir");
        let signed_client = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"signed manifest".as_slice()),
            ("META-INF/MOJANG_C.SF", b"signature".as_slice()),
            ("META-INF/MOJANG_C.RSA", b"signature".as_slice()),
            ("net/minecraft/client/Minecraft.class", b"class".as_slice()),
        ]);
        fs::write(
            base_dir.join(format!("{minecraft_version}.jar")),
            &signed_client,
        )
        .expect("write signed base client");
        fs::write(
            base_dir.join(format!("{minecraft_version}.json")),
            format!(
                r#"{{
                    "id":"{minecraft_version}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.Minecraft",
                    "assetIndex":{{"id":"legacy","url":"","sha1":"","size":0,"totalSize":0}},
                    "downloads":{{
                        "client":{{
                            "url":"https://example.invalid/{minecraft_version}.jar",
                            "sha1":"{}",
                            "size":{}
                        }}
                    }},
                    "libraries":[]
                }}"#,
                sha1_hex(&signed_client),
                signed_client.len()
            ),
        )
        .expect("write base version json");
        let install_profile = br#"{
            "versionInfo": {
                "id": "1.5.2-Forge7.8.1.738",
                "mainClass": "net.minecraft.launchwrapper.Launch",
                "minecraftArguments": "${auth_player_name} ${auth_session}",
                "assetIndex": { "id": "legacy" },
                "libraries": [
                    { "name": "net.minecraftforge:minecraftforge:7.8.1.738" }
                ]
            },
            "install": {
                "path": "net.minecraftforge:minecraftforge:7.8.1.738",
                "filePath": "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                "target": "1.5.2-Forge7.8.1.738",
                "minecraft": "1.5.2",
                "stripMeta": true
            }
        }"#;
        let forge_jar = zip_entries(&[
            ("META-INF/MANIFEST.MF", b"forge manifest".as_slice()),
            ("META-INF/FORGE.SF", b"signature".as_slice()),
            ("net/minecraftforge/Forge.class", b"forge".as_slice()),
        ]);
        let installer = zip_entries(&[
            ("install_profile.json", install_profile.as_slice()),
            (
                "minecraftforge-universal-1.5.2-7.8.1.738.jar",
                forge_jar.as_slice(),
            ),
        ]);
        let mut record = installer_record();
        record.minecraft_version = minecraft_version.to_string();
        record.loader_version = loader_version.to_string();
        canonicalize_record_identity(&mut record);
        assert_eq!(record.version_id, version_id);
        record.strategy = LoaderInstallStrategy::ForgeLegacyInstaller;
        let installer_server = TestByteServer::start_with_sha1(installer);
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: crate::paths::loader_work_dir(&root).join(&record.version_id),
        };
        let installer_source =
            verified_test_source(&installer_server.url, "loader installer").await;

        install_installer_source_after_authenticated_base(
            &root,
            &plan,
            installer_source,
            &test_client_integrity(&root, &record.minecraft_version),
            &mut |_| {},
        )
        .await
        .expect("install stripMeta legacy installer");

        let child_jar = versions_dir(&root)
            .join(&version_id)
            .join(format!("{version_id}.jar"));
        assert!(zip_contains(
            &child_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&child_jar, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&child_jar, "META-INF/MOJANG_C.SF"));
        assert!(!zip_contains(&child_jar, "META-INF/MOJANG_C.RSA"));
        let base_jar = base_dir.join(format!("{minecraft_version}.jar"));
        assert!(zip_contains(&base_jar, "META-INF/MANIFEST.MF"));
        assert!(zip_contains(&base_jar, "META-INF/MOJANG_C.SF"));
        assert!(zip_contains(&base_jar, "META-INF/MOJANG_C.RSA"));
        let forge_artifact = root
            .join("libraries")
            .join("net")
            .join("minecraftforge")
            .join("forge")
            .join("1.5.2-7.8.1.738")
            .join("forge-1.5.2-7.8.1.738-universal.jar");
        assert!(zip_contains(
            &forge_artifact,
            "net/minecraftforge/Forge.class"
        ));
        assert!(!zip_contains(&forge_artifact, "META-INF/MANIFEST.MF"));
        assert!(!zip_contains(&forge_artifact, "META-INF/FORGE.SF"));

        let child_jar_bytes = fs::read(&child_jar).expect("read child jar");
        let version_json = fs::read_to_string(
            versions_dir(&root)
                .join(&version_id)
                .join(format!("{version_id}.json")),
        )
        .expect("read version json");
        let version: serde_json::Value =
            serde_json::from_str(&version_json).expect("parse version json");
        assert_eq!(
            version["downloads"]["client"]["sha1"],
            sha1_hex(&child_jar_bytes)
        );
        assert_eq!(
            version["downloads"]["client"]["size"],
            child_jar_bytes.len() as i64
        );
        assert_eq!(version["downloads"]["client"]["url"], "");

        assert_eq!(installer_server.request_count(), 2);
        installer_server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_overlays_base_client_without_mutating_base() {
        let root = temp_dir("legacy-archive-overlay");
        let base_version_id = "1.2.5";
        let base_dir = versions_dir(&root).join(base_version_id);
        fs::create_dir_all(&base_dir).expect("base version dir");
        fs::write(
            base_dir.join(format!("{base_version_id}.json")),
            format!(
                r#"{{
                    "id":"{base_version_id}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.Minecraft",
                    "assetIndex":{{"id":"legacy","url":"","sha1":"","size":0,"totalSize":0}},
                    "libraries":[]
                }}"#
            ),
        )
        .expect("base json");
        fs::write(
            base_dir.join(format!("{base_version_id}.jar")),
            zip_entries(&[
                ("net/minecraft/client/Minecraft.class", b"base".as_slice()),
                ("com/example/Replaced.class", b"base".as_slice()),
            ]),
        )
        .expect("base jar");
        let forge_archive = zip_entries(&[
            ("net/minecraftforge/Forge.class", b"forge".as_slice()),
            ("com/example/Replaced.class", b"forge".as_slice()),
            ("META-INF/TEST.SF", b"signature".as_slice()),
        ]);
        let server = TestByteServer::start_with_sha1(forge_archive.clone());
        let mut record = legacy_archive_record();
        record.minecraft_version = base_version_id.to_string();
        canonicalize_record_identity(&mut record);
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };

        let archive_source = verified_test_source(&server.url, "legacy Forge archive").await;
        let base_receipt = test_authenticated_receipt(&root, &record.minecraft_version);
        let receipt = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_progress| {},
        )
        .await
        .expect("install legacy archive");

        assert_eq!(receipt.version_id(), record.version_id);
        let installed_jar = versions_dir(&root)
            .join(&record.version_id)
            .join(format!("{}.jar", record.version_id));
        assert!(zip_contains(
            &installed_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(zip_contains(
            &installed_jar,
            "net/minecraftforge/Forge.class"
        ));
        assert_eq!(
            zip_entry_bytes(&installed_jar, "com/example/Replaced.class"),
            b"forge"
        );
        assert!(!zip_contains(&installed_jar, "META-INF/TEST.SF"));
        let installed_jar_bytes = fs::read(&installed_jar).expect("read installed jar");
        let expected_child_bytes = overlay_legacy_archive_bytes(
            &fs::read(base_dir.join(format!("{base_version_id}.jar")))
                .expect("read authenticated base source"),
            &forge_archive,
        )
        .expect("derive expected child source");
        assert_eq!(installed_jar_bytes, expected_child_bytes);
        let installed_jar_receipt = receipt
            .into_inventory()
            .entries()
            .iter()
            .find(|entry| entry.kind() == KnownGoodArtifactKind::ClientJar)
            .expect("client jar receipt")
            .integrity()
            .clone();
        let KnownGoodIntegrity::Sha1 { digest, size } = installed_jar_receipt else {
            panic!("client jar receipt must retain canonical source integrity");
        };
        assert_eq!(digest.as_str(), sha1_hex(&installed_jar_bytes));
        assert_eq!(size, Some(installed_jar_bytes.len() as u64));
        let installed_version_json = fs::read_to_string(
            versions_dir(&root)
                .join(&record.version_id)
                .join(format!("{}.json", record.version_id)),
        )
        .expect("read installed version json");
        let installed_version: serde_json::Value =
            serde_json::from_str(&installed_version_json).expect("parse installed version json");
        assert_eq!(
            installed_version["downloads"]["client"]["sha1"],
            sha1_hex(&installed_jar_bytes)
        );
        assert_eq!(
            installed_version["downloads"]["client"]["size"],
            installed_jar_bytes.len() as i64
        );
        assert_eq!(installed_version["downloads"]["client"]["url"], "");

        let base_jar = base_dir.join(format!("{base_version_id}.jar"));
        assert!(zip_contains(
            &base_jar,
            "net/minecraft/client/Minecraft.class"
        ));
        assert!(!zip_contains(&base_jar, "net/minecraftforge/Forge.class"));
        assert_eq!(
            zip_entry_bytes(&base_jar, "com/example/Replaced.class"),
            b"base"
        );

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_corrupt_authenticated_base_client() {
        let root = temp_dir("legacy-archive-corrupt-base");
        let base_version_id = "1.2.5";
        write_base_version(&root, base_version_id);
        let base_receipt = test_authenticated_receipt(&root, base_version_id);
        fs::write(
            versions_dir(&root)
                .join(base_version_id)
                .join(format!("{base_version_id}.jar")),
            b"corrupt base client",
        )
        .expect("corrupt base client");
        let server = TestByteServer::start_with_sha1(zip_entries(&[(
            "net/minecraftforge/Forge.class",
            b"forge".as_slice(),
        )]));
        let record = legacy_archive_record();
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let archive_source = verified_test_source(&server.url, "legacy Forge archive").await;

        let error = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_| {},
        )
        .await
        .expect_err("corrupt base must fail");

        assert!(
            matches!(error, LoaderError::Verify(message) if message.contains("authenticate base client"))
        );
        assert!(!versions_dir(&root).join(record.version_id).exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_mismatched_live_sha1_proof() {
        let root = temp_dir("legacy-archive-sha1-mismatch");
        let archive = zip_entries(&[("net/minecraftforge/Forge.class", b"forge".as_slice())]);
        let server = TestByteServer::start_with_sha1_proof(archive, vec![b'0'; 40]);
        let record = legacy_archive_record();
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let error = install_from_legacy_archive(&root, &plan, &server.url, &mut |_| {})
            .await
            .expect_err("mismatched proof must fail");

        assert!(
            matches!(error, LoaderError::Verify(message) if message.contains("live sha1 proof"))
        );
        assert!(!root.exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn legacy_archive_rejects_malformed_live_sha1_proof() {
        let root = temp_dir("legacy-archive-sha1-malformed");
        let archive = zip_entries(&[("net/minecraftforge/Forge.class", b"forge".as_slice())]);
        let server = TestByteServer::start_with_sha1_proof(
            archive,
            b"not-a-strict-sha1 artifact.jar".to_vec(),
        );
        let record = legacy_archive_record();
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };

        let error = install_from_legacy_archive(&root, &plan, &server.url, &mut |_| {})
            .await
            .expect_err("malformed proof must fail");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("exactly one 40-hex digest"))
        );
        assert!(!root.exists());
        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn legacy_archive_rejects_symlinked_child_version_without_outside_write() {
        let root = temp_dir("legacy-archive-symlink-child");
        let outside = temp_dir("legacy-archive-symlink-outside");
        let sentinel = outside.join("sentinel");
        fs::create_dir_all(&outside).expect("outside dir");
        fs::write(&sentinel, b"untouched").expect("outside sentinel");
        let base_version_id = "1.2.5";
        write_base_version(&root, base_version_id);
        fs::write(
            versions_dir(&root)
                .join(base_version_id)
                .join(format!("{base_version_id}.jar")),
            zip_entries(&[("net/minecraft/client/Minecraft.class", b"base".as_slice())]),
        )
        .expect("valid base client");
        let base_receipt = test_authenticated_receipt(&root, base_version_id);
        let record = legacy_archive_record();
        let child_path = versions_dir(&root).join(&record.version_id);
        std::os::unix::fs::symlink(&outside, &child_path).expect("symlink child version");
        let server = TestByteServer::start_with_sha1(zip_entries(&[(
            "net/minecraftforge/Forge.class",
            b"forge".as_slice(),
        )]));
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let archive_source = verified_test_source(&server.url, "legacy Forge archive").await;

        let error = install_legacy_archive_after_authenticated_base(
            &root,
            &plan,
            archive_source,
            &base_receipt,
            &mut |_| {},
        )
        .await
        .expect_err("symlinked child must fail");

        assert!(
            matches!(&error, LoaderError::Verify(message) if message.contains("exact managed directory")),
            "unexpected error: {error:?}"
        );
        assert_eq!(fs::read(&sentinel).expect("read sentinel"), b"untouched");
        assert_eq!(fs::read_dir(&outside).expect("outside dir").count(), 1);
        server.stop();
        let _ = fs::remove_dir_all(root);
        let _ = fs::remove_dir_all(outside);
    }

    #[test]
    fn legacy_overlay_rejects_declared_entry_expansion_over_limit() {
        let base_client =
            zip_entries(&[("net/minecraft/client/Minecraft.class", b"base".as_slice())]);
        let mut archive = zip_entries(&[("oversized.class", b"".as_slice())]);
        set_first_zip_entry_declared_size(
            &mut archive,
            u32::try_from(super::MAX_LEGACY_OVERLAY_ENTRY_BYTES + 1)
                .expect("test limit fits zip32"),
        );

        let error = overlay_legacy_archive_bytes(&base_client, &archive)
            .expect_err("declared expansion must be rejected before decompression");

        assert!(
            matches!(error, LoaderError::InvalidProfile(message) if message.contains("bounded output limits"))
        );
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }

    fn installer_jar(version_id: &str) -> Vec<u8> {
        installer_jar_with_profile_json(&profile_json(version_id))
    }

    fn installer_jar_with_profile_json(profile_json: &[u8]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        archive
            .start_file("version.json", SimpleFileOptions::default())
            .expect("start version.json");
        archive.write_all(profile_json).expect("write version.json");
        archive.finish().expect("finish installer jar");
        cursor.into_inner()
    }

    fn zip_entries(entries: &[(&str, &[u8])]) -> Vec<u8> {
        use zip::write::SimpleFileOptions;

        let mut cursor = std::io::Cursor::new(Vec::new());
        let mut archive = zip::ZipWriter::new(&mut cursor);
        for (name, bytes) in entries {
            archive
                .start_file(name, SimpleFileOptions::default())
                .expect("start zip entry");
            archive.write_all(bytes).expect("write zip entry");
        }
        archive.finish().expect("finish zip");
        cursor.into_inner()
    }

    fn set_first_zip_entry_declared_size(bytes: &mut [u8], size: u32) {
        let local = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x03, 0x04])
            .expect("local zip header");
        let central = bytes
            .windows(4)
            .position(|window| window == [0x50, 0x4b, 0x01, 0x02])
            .expect("central zip header");
        bytes[local + 22..local + 26].copy_from_slice(&size.to_le_bytes());
        bytes[central + 24..central + 28].copy_from_slice(&size.to_le_bytes());
    }

    fn zip_contains(path: &std::path::Path, name: &str) -> bool {
        let file = fs::File::open(path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        archive.by_name(name).is_ok()
    }

    fn zip_entry_bytes(path: &std::path::Path, name: &str) -> Vec<u8> {
        let file = fs::File::open(path).expect("open zip");
        let mut archive = zip::ZipArchive::new(file).expect("read zip");
        let mut entry = archive.by_name(name).expect("zip entry");
        let mut bytes = Vec::new();
        entry.read_to_end(&mut bytes).expect("read zip entry");
        bytes
    }

    fn profile_json(id: &str) -> Vec<u8> {
        format!(r#"{{"id":"{id}","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}"#)
            .into_bytes()
    }

    fn write_base_version(root: &std::path::Path, version_id: &str) {
        let version_dir = versions_dir(root).join(version_id);
        fs::create_dir_all(&version_dir).expect("create base version dir");
        fs::write(
            version_dir.join(format!("{version_id}.json")),
            format!(
                r#"{{
                    "id":"{version_id}",
                    "type":"release",
                    "mainClass":"net.minecraft.client.main.Main",
                    "assetIndex":{{"id":"{version_id}","url":"","sha1":"","size":0,"totalSize":0}},
                    "libraries":[]
                }}"#
            ),
        )
        .expect("write base version json");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write base jar");
    }

    fn test_client_integrity(root: &std::path::Path, version_id: &str) -> ExpectedIntegrity {
        let client = fs::read(
            versions_dir(root)
                .join(version_id)
                .join(format!("{version_id}.jar")),
        )
        .expect("read test base client");
        ExpectedIntegrity {
            size: Some(client.len() as u64),
            sha1: Some(sha1_hex(&client)),
        }
    }

    async fn verified_test_source(url: &str, label: &'static str) -> VerifiedLoaderSource {
        fetch_sha1_verified_source(url, super::MAX_INSTALLER_DOWNLOAD_SIZE, label)
            .await
            .expect("verified test source")
    }

    fn test_authenticated_receipt(
        root: &std::path::Path,
        version_id: &str,
    ) -> KnownGoodInstallReceipt {
        let integrity = test_client_integrity(root, version_id);
        let mut version = resolve_version(root, version_id).expect("resolve test base version");
        let client = version.downloads.client.get_or_insert_default();
        client.size = integrity.size.expect("test client size") as i64;
        client.sha1 = integrity.sha1.expect("test client sha1");
        client.url = "https://example.invalid/client.jar".to_string();
        KnownGoodInstallReceipt::from_test_authenticated_version(version, default_environment())
    }

    fn assert_backend_version_was_written(
        root: &std::path::Path,
        backend_version_id: &str,
        upstream_version_id: &str,
    ) {
        let backend_dir = versions_dir(root).join(backend_version_id);
        let upstream_dir = versions_dir(root).join(upstream_version_id);
        assert!(backend_dir.is_dir());
        assert!(!upstream_dir.exists());
        let version_json = fs::read(backend_dir.join(format!("{backend_version_id}.json")))
            .expect("read backend version json");
        let version: serde_json::Value =
            serde_json::from_slice(&version_json).expect("parse backend version json");
        assert_eq!(
            version.get("id").and_then(serde_json::Value::as_str),
            Some(backend_version_id)
        );
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn profile_record() -> LoaderBuildRecord {
        let component_id = LoaderComponentId::Fabric;
        let minecraft_version = "1.21.5";
        let loader_version = "0.16.14";
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id,
            component_name: "Fabric".to_string(),
            build_id: build_id_for(component_id, minecraft_version, loader_version),
            minecraft_version: minecraft_version.to_string(),
            loader_version: loader_version.to_string(),
            version_id: installed_version_id_for(component_id, minecraft_version, loader_version)
                .expect("canonical installed version id"),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://meta.fabricmc.net/profile/json".to_string(),
            },
        }
    }

    fn fabric_profile_proof(record: &LoaderBuildRecord) -> ProfileInstallProof {
        ProfileInstallProof {
            canonical_profile_id: format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            inherits_from: record.minecraft_version.clone(),
            client_main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            required_libraries: vec![
                ProfileLibraryProof {
                    coordinate: format!("net.fabricmc:fabric-loader:{}", record.loader_version),
                    sha1: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
                    size: Some(42),
                },
                ProfileLibraryProof {
                    coordinate: format!("net.fabricmc:intermediary:{}", record.minecraft_version),
                    sha1: None,
                    size: None,
                },
            ],
        }
    }

    fn fabric_profile_fragment(record: &LoaderBuildRecord) -> LoaderProfileFragment {
        LoaderProfileFragment {
            id: format!(
                "fabric-loader-{}-{}",
                record.loader_version, record.minecraft_version
            ),
            inherits_from: record.minecraft_version.clone(),
            kind: "release".to_string(),
            main_class: "net.fabricmc.loader.impl.launch.knot.KnotClient".to_string(),
            libraries: vec![
                Library {
                    name: format!("net.fabricmc:fabric-loader:{}", record.loader_version),
                    ..Library::default()
                },
                Library {
                    name: format!("net.fabricmc:intermediary:{}", record.minecraft_version),
                    ..Library::default()
                },
            ],
            ..LoaderProfileFragment::default()
        }
    }

    fn installer_record() -> LoaderBuildRecord {
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Forge;
        record.component_name = "Forge".to_string();
        record.loader_version = "55.0.0".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::ForgeModern;
        record.artifact_kind = LoaderArtifactKind::InstallerJar;
        record.install_source = LoaderInstallSource::InstallerJar {
            url: "https://example.test/installer.jar".to_string(),
        };
        record
    }

    fn legacy_archive_record() -> LoaderBuildRecord {
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Forge;
        record.component_name = "Forge".to_string();
        record.minecraft_version = "1.2.5".to_string();
        record.loader_version = "3.4.9.171".to_string();
        canonicalize_record_identity(&mut record);
        record.strategy = LoaderInstallStrategy::ForgeEarliestLegacy;
        record.artifact_kind = LoaderArtifactKind::LegacyArchive;
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: "https://example.test/legacy.jar".to_string(),
        };
        record
    }

    fn canonicalize_record_identity(record: &mut LoaderBuildRecord) {
        record.build_id = build_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        );
        record.version_id = installed_version_id_for(
            record.component_id,
            &record.minecraft_version,
            &record.loader_version,
        )
        .expect("canonical installed version id");
    }

    struct TestByteServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        stop_server: mpsc::Sender<()>,
        server: thread::JoinHandle<()>,
    }

    impl TestByteServer {
        fn start(body: Vec<u8>) -> Self {
            Self::start_with_optional_sha1(body, None)
        }

        fn start_with_sha1(body: Vec<u8>) -> Self {
            let proof = sha1_hex(&body).into_bytes();
            Self::start_with_optional_sha1(body, Some(proof))
        }

        fn start_with_sha1_proof(body: Vec<u8>, proof: Vec<u8>) -> Self {
            Self::start_with_optional_sha1(body, Some(proof))
        }

        fn start_with_optional_sha1(body: Vec<u8>, sha1_proof: Option<Vec<u8>>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
            listener
                .set_nonblocking(true)
                .expect("set test server nonblocking");
            let url = format!(
                "http://{}/legacy-client.zip",
                listener.local_addr().expect("server addr")
            );
            let request_count = Arc::new(AtomicUsize::new(0));
            let server_request_count = Arc::clone(&request_count);
            let (stop_server, server_stopped) = mpsc::channel();
            let server = thread::spawn(move || {
                loop {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            server_request_count.fetch_add(1, Ordering::SeqCst);
                            respond_ok(stream, &body, sha1_proof.as_deref());
                        }
                        Err(error) if error.kind() == ErrorKind::WouldBlock => {
                            if server_stopped.try_recv().is_ok() {
                                break;
                            }
                            thread::sleep(Duration::from_millis(10));
                        }
                        Err(error) => panic!("accept connection: {error}"),
                    }
                }
            });

            Self {
                url,
                request_count,
                stop_server,
                server,
            }
        }

        fn request_count(&self) -> usize {
            self.request_count.load(Ordering::SeqCst)
        }

        fn stop(self) {
            self.stop_server.send(()).expect("stop test server");
            self.server.join().expect("server thread");
        }
    }

    fn respond_ok(mut stream: TcpStream, body: &[u8], sha1_proof: Option<&[u8]>) {
        let mut buffer = [0_u8; 1024];
        let read = stream.read(&mut buffer).unwrap_or_default();
        let request = String::from_utf8_lossy(&buffer[..read]);
        let body = if request.lines().next().is_some_and(|line| {
            line.split_whitespace()
                .nth(1)
                .is_some_and(|path| path.ends_with(".sha1"))
        }) {
            sha1_proof.unwrap_or(body)
        } else {
            body
        };
        let header = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        stream
            .write_all(header.as_bytes())
            .expect("write response header");
        stream.write_all(body).expect("write response body");
    }
}
