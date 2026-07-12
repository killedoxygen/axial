#[cfg(test)]
use crate::download::download_libraries_with_facts_and_descriptors;
use crate::download::{
    DownloadError, DownloadProgress, Downloader, ExecutionDownloadError, ExecutionDownloadReport,
    LauncherManagedArtifactReadiness,
    download_libraries_allowing_missing_checksums_with_facts_and_descriptors, library_jobs_for,
    verify_existing_launcher_managed_artifact, write_launcher_managed_artifact_bytes_to_temp,
};
use crate::launch::{DownloadEntry, VersionJson, resolve_version};
use crate::loaders::compose::{
    LoaderProfileFragment, cleanup_incomplete_version, compose_loader_version,
    finalize_version_install, write_composed_version,
};
use crate::loaders::forge_installer::{
    ExtractedForgeInstaller, extract_installer, extract_maven_entries,
};
use crate::loaders::http::fetch_bytes;
use crate::loaders::processors::run_processors;
use crate::loaders::types::{LoaderBuildRecord, LoaderError, LoaderInstallPlan};
use crate::loaders::{validate_provider_version_id, validate_version_id};
use crate::paths::{loader_artifacts_dir, versions_dir};
use crate::profiles::ensure_launcher_profiles;
use serde::Serialize;
use sha1::{Digest as _, Sha1};
use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write, sink};
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;
use zip::ZipArchive;
use zip::ZipWriter;
use zip::result::ZipError;
use zip::write::SimpleFileOptions;

const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;
const LOADER_METADATA_FILE: &str = ".axial-loader.json";

#[derive(Debug)]
struct CachedProfile {
    bytes: Vec<u8>,
    fragment: LoaderProfileFragment,
}

#[derive(Debug, Serialize)]
struct InstalledLoaderMetadata<'a> {
    schema_version: u32,
    component_id: crate::loaders::types::LoaderComponentId,
    component_name: &'a str,
    build_id: &'a str,
    minecraft_version: &'a str,
    loader_version: &'a str,
    build_meta: &'a crate::loaders::types::LoaderBuildMetadata,
}

// Profile-source loaders ship a ready version JSON and then download its libraries.
pub async fn install_from_profile_source<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    profile_url: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    send(progress(
        "profile",
        0,
        1,
        Some("Fetching loader profile...".to_string()),
    ));
    let profile_cache_path = cached_profile_path(library_dir, &plan.record);
    let cached_profile = read_valid_profile_json(
        &profile_cache_path,
        profile_url,
        &plan.record.component_name,
    )
    .await?;
    let profile_bytes = cached_profile.bytes;
    let mut fragment = cached_profile.fragment;
    mark_loader_libraries_checksumless_allowed(&mut fragment.libraries);
    if !fragment.id.trim().is_empty() {
        validate_provider_version_id(&fragment.id, "upstream loader profile version id")?;
    }
    let installed_version_id = plan.record.version_id.clone();
    validate_version_id(&installed_version_id, "installed loader version id")?;

    cleanup_on_error(
        write_raw_profile_version(library_dir, &installed_version_id, &profile_bytes).await,
        library_dir,
        &installed_version_id,
    )?;
    let library_download_result = Box::pin(download_profile_loader_libraries_with_evidence(
        library_dir,
        &fragment.libraries,
        "loader_libraries",
        &mut *send,
    ))
    .await;
    cleanup_on_error(library_download_result, library_dir, &installed_version_id)?;
    cleanup_on_error(
        Box::pin(ensure_base_version(
            library_dir,
            &plan.record.minecraft_version,
            send,
        ))
        .await,
        library_dir,
        &installed_version_id,
    )?;

    let version = cleanup_on_error(
        compose_loader_version(
            library_dir,
            &plan.record.minecraft_version,
            &installed_version_id,
            &fragment,
        ),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &installed_version_id,
            &version,
            &plan.record.minecraft_version,
        )
        .await,
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
        write_installed_loader_metadata(library_dir, &installed_version_id, &plan.record).await,
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
    Box::pin(ensure_base_version(
        library_dir,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    let installer_path = cached_installer_path(library_dir, &plan.record);

    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} installer...",
            plan.record.component_name
        )),
    ));
    let (installer_data, mut extracted) =
        read_valid_installer(&installer_path, installer_url, &plan.record.component_name).await?;
    mark_loader_libraries_checksumless_allowed(&mut extracted.version_fragment.libraries);
    mark_loader_libraries_checksumless_allowed(&mut extracted.libraries);

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
    let version = compose_loader_version(
        library_dir,
        &plan.record.minecraft_version,
        &installed_version_id,
        &extracted.version_fragment,
    )?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &installed_version_id,
            &version,
            &plan.record.minecraft_version,
        )
        .await,
        library_dir,
        &installed_version_id,
    )?;
    let installer_data = cleanup_on_error(
        extract_maven_entries_blocking(
            installer_data,
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
            &installer_data,
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
        write_installed_loader_metadata(library_dir, &installed_version_id, &plan.record).await,
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
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    Box::pin(ensure_base_version(
        library_dir,
        &plan.record.minecraft_version,
        send,
    ))
    .await?;
    validate_version_id(&plan.record.version_id, "installed loader version id")?;

    let archive_path = cached_legacy_archive_path(library_dir, &plan.record);
    send(progress(
        "artifacts",
        0,
        1,
        Some(format!(
            "Downloading {} archive...",
            plan.record.component_name
        )),
    ));
    let archive_data =
        read_valid_legacy_archive(&archive_path, archive_url, &plan.record.component_name).await?;

    let fragment = LoaderProfileFragment {
        id: plan.record.version_id.clone(),
        inherits_from: plan.record.minecraft_version.clone(),
        ..LoaderProfileFragment::default()
    };
    let version = compose_loader_version(
        library_dir,
        &plan.record.minecraft_version,
        &plan.record.version_id,
        &fragment,
    )?;
    cleanup_on_error(
        write_composed_version(
            library_dir,
            &plan.record.version_id,
            &version,
            &plan.record.minecraft_version,
        )
        .await,
        library_dir,
        &plan.record.version_id,
    )?;

    cleanup_on_error(
        overlay_legacy_archive_onto_base_client(
            library_dir,
            &plan.record.minecraft_version,
            &plan.record.version_id,
            archive_data,
        )
        .await,
        library_dir,
        &plan.record.version_id,
    )?;
    cleanup_on_error(
        write_patched_client_jar_integrity(library_dir, &plan.record.version_id).await,
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
        write_installed_loader_metadata(library_dir, &plan.record.version_id, &plan.record).await,
        library_dir,
        &plan.record.version_id,
    )?;
    cleanup_on_error(
        finalize_version_install(library_dir, &plan.record.version_id),
        library_dir,
        &plan.record.version_id,
    )?;
    send(done());
    Ok(plan.record.version_id.clone())
}

async fn ensure_base_version<F>(
    library_dir: &Path,
    version_id: &str,
    send: &mut F,
) -> Result<(), LoaderError>
where
    F: FnMut(DownloadProgress),
{
    if is_base_game_installed(library_dir, version_id).await {
        return Ok(());
    }

    let install_lock = base_version_install_lock(library_dir, version_id);
    let _guard = install_lock.lock().await;
    if is_base_game_installed(library_dir, version_id).await {
        return Ok(());
    }

    let downloader = Downloader::new(library_dir.to_path_buf());
    let mut facts = Vec::new();
    let mut descriptors = Vec::new();
    let result = Box::pin(downloader.install_version_with_facts_and_descriptors(
        version_id,
        None,
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
        Ok(()) => Ok(()),
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

async fn is_base_game_installed(library_dir: &Path, game_version: &str) -> bool {
    let version_dir = versions_dir(library_dir).join(game_version);
    let json_path = version_dir.join(format!("{game_version}.json"));
    let jar_path = version_dir.join(format!("{game_version}.jar"));
    let marker_path = version_dir.join(".incomplete");
    if !json_path.is_file() || !jar_path.is_file() || marker_path.exists() {
        return false;
    }

    let Ok(version) = resolve_version(library_dir, game_version) else {
        return false;
    };
    for job in library_jobs_for(
        library_dir,
        &version.libraries,
        &crate::rules::default_environment(),
    ) {
        if verify_existing_launcher_managed_artifact_on_blocking_thread(job.path, job.expected)
            .await
            != LauncherManagedArtifactReadiness::Verified
        {
            return false;
        }
    }
    true
}

async fn verify_existing_launcher_managed_artifact_on_blocking_thread(
    path: PathBuf,
    expected: crate::download::ExpectedIntegrity,
) -> LauncherManagedArtifactReadiness {
    tokio::task::spawn_blocking(move || verify_existing_launcher_managed_artifact(&path, &expected))
        .await
        .unwrap_or(LauncherManagedArtifactReadiness::Corrupt)
}

fn mark_loader_libraries_checksumless_allowed(libraries: &mut [crate::launch::Library]) {
    for library in libraries {
        library.axial_checksumless_allowed = true;
    }
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
    validate_version_id(version_id, "installed loader version id")?;
    let metadata = InstalledLoaderMetadata {
        schema_version: 1,
        component_id: record.component_id,
        component_name: &record.component_name,
        build_id: &record.build_id,
        minecraft_version: &record.minecraft_version,
        loader_version: &record.loader_version,
        build_meta: &record.build_meta,
    };
    let path = versions_dir(library_dir)
        .join(version_id)
        .join(LOADER_METADATA_FILE);
    async_fs::write(path, serde_json::to_vec_pretty(&metadata)?).await?;
    Ok(())
}

async fn write_raw_profile_version(
    library_dir: &Path,
    version_id: &str,
    profile_bytes: &[u8],
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    let version_dir = versions_dir(library_dir).join(version_id);
    async_fs::create_dir_all(&version_dir).await?;
    async_fs::write(version_dir.join(".incomplete"), b"installing").await?;
    async_fs::write(
        version_dir.join(format!("{version_id}.json")),
        profile_bytes,
    )
    .await?;
    Ok(())
}

async fn download_to_memory(url: &str) -> Result<Vec<u8>, LoaderError> {
    fetch_bytes(url, MAX_INSTALLER_DOWNLOAD_SIZE).await
}

async fn read_valid_installer(
    path: &Path,
    url: &str,
    component_name: &str,
) -> Result<(Vec<u8>, ExtractedForgeInstaller), LoaderError> {
    if path_is_file(path).await
        && let Some(bytes) = read_cached_artifact(path).await?
    {
        return extract_installer_blocking(bytes, component_name.to_string()).await;
    }

    let bytes = download_to_memory(url).await?;
    let (installer_data, extracted) =
        extract_installer_blocking(bytes, component_name.to_string()).await?;
    Box::pin(write_cached_artifact(path, &installer_data)).await?;
    Ok((installer_data, extracted))
}

async fn read_valid_profile_json(
    path: &Path,
    url: &str,
    component_name: &str,
) -> Result<CachedProfile, LoaderError> {
    if path_is_file(path).await {
        let bytes = match read_cached_artifact(path).await? {
            Some(bytes) => bytes,
            None => {
                let bytes = download_to_memory(url).await?;
                let fragment = parse_profile_json(&bytes, component_name)?;
                let _ = Box::pin(write_cached_artifact(path, &bytes)).await;
                return Ok(CachedProfile { bytes, fragment });
            }
        };
        match parse_profile_json(&bytes, component_name) {
            Ok(fragment) => return Ok(CachedProfile { bytes, fragment }),
            Err(error) => return Err(error),
        }
    }

    let bytes = download_to_memory(url).await?;
    let fragment = parse_profile_json(&bytes, component_name)?;
    let _ = Box::pin(write_cached_artifact(path, &bytes)).await;
    Ok(CachedProfile { bytes, fragment })
}

fn parse_profile_json(
    bytes: &[u8],
    component_name: &str,
) -> Result<LoaderProfileFragment, LoaderError> {
    serde_json::from_slice::<LoaderProfileFragment>(bytes)
        .map_err(|error| LoaderError::InvalidProfile(format!("{component_name} profile: {error}")))
}

async fn read_valid_legacy_archive(
    path: &Path,
    url: &str,
    component_name: &str,
) -> Result<Vec<u8>, LoaderError> {
    if path_is_file(path).await
        && let Some(bytes) = read_cached_artifact(path).await?
    {
        match validate_legacy_archive(&bytes) {
            Ok(()) => return Ok(bytes),
            Err(error) => return Err(legacy_archive_error(component_name, error)),
        }
    }

    let bytes = download_to_memory(url).await?;
    if let Err(error) = validate_legacy_archive(&bytes) {
        return Err(legacy_archive_error(component_name, error));
    }
    Box::pin(write_cached_artifact(path, &bytes)).await?;
    Ok(bytes)
}

async fn read_cached_artifact(path: &Path) -> Result<Option<Vec<u8>>, LoaderError> {
    let metadata = async_fs::metadata(path).await?;
    if metadata.len() > MAX_INSTALLER_DOWNLOAD_SIZE {
        return Err(LoaderError::Verify(
            "cached loader artifact exceeded the bounded size limit".to_string(),
        ));
    }
    Ok(Some(async_fs::read(path).await?))
}

fn validate_legacy_archive(bytes: &[u8]) -> Result<(), ZipError> {
    let mut archive = ZipArchive::new(std::io::Cursor::new(bytes))?;
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index)?;
        std::io::copy(&mut entry, &mut sink()).map_err(ZipError::Io)?;
    }
    Ok(())
}

async fn extract_installer_blocking(
    installer_data: Vec<u8>,
    component_name: String,
) -> Result<(Vec<u8>, ExtractedForgeInstaller), LoaderError> {
    tokio::task::spawn_blocking(move || {
        let extracted = extract_installer(&installer_data)
            .map_err(|error| installer_extract_error(&component_name, error))?;
        Ok((installer_data, extracted))
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn extract_maven_entries_blocking(
    installer_data: Vec<u8>,
    library_dir: PathBuf,
    component_name: String,
) -> Result<Vec<u8>, LoaderError> {
    tokio::task::spawn_blocking(move || {
        extract_maven_entries(&installer_data, &library_dir)
            .map_err(|error| installer_extract_error(&component_name, error))?;
        Ok(installer_data)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))?
}

async fn overlay_legacy_archive_onto_base_client(
    library_dir: &Path,
    base_version_id: &str,
    version_id: &str,
    archive_data: Vec<u8>,
) -> Result<(), LoaderError> {
    validate_version_id(base_version_id, "base minecraft version id")?;
    validate_version_id(version_id, "installed loader version id")?;
    let base_jar = versions_dir(library_dir)
        .join(base_version_id)
        .join(format!("{base_version_id}.jar"));
    let output_jar = versions_dir(library_dir)
        .join(version_id)
        .join(format!("{version_id}.jar"));
    let temp_jar = artifact_tmp_path(&output_jar);
    let blocking_temp_jar = temp_jar.clone();
    tokio::task::spawn_blocking(move || {
        overlay_legacy_archive_blocking(&base_jar, &blocking_temp_jar, &archive_data)
    })
    .await
    .map_err(|error| LoaderError::InstallExecutionFailed(error.to_string()))??;
    async_fs::rename(&temp_jar, &output_jar).await?;
    Ok(())
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

fn overlay_legacy_archive_blocking(
    base_jar: &Path,
    temp_jar: &Path,
    archive_data: &[u8],
) -> Result<(), LoaderError> {
    if let Some(parent) = temp_jar.parent() {
        std::fs::create_dir_all(parent)?;
    }

    let base_file = File::open(base_jar)?;
    let mut base_archive = ZipArchive::new(base_file)
        .map_err(|error| legacy_archive_error("base Minecraft", error))?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    let forge_names = archive_entry_names(&mut forge_archive)?;
    let output_file = File::create(temp_jar)?;
    let mut writer = ZipWriter::new(output_file);

    copy_zip_entries(&mut base_archive, &mut writer, Some(&forge_names))?;
    let mut forge_archive = ZipArchive::new(std::io::Cursor::new(archive_data))
        .map_err(|error| legacy_archive_error("Forge", error))?;
    copy_zip_entries(&mut forge_archive, &mut writer, None)?;
    writer
        .finish()
        .map_err(|error| legacy_archive_error("legacy Forge overlay", error))?;
    Ok(())
}

fn archive_entry_names<R: std::io::Read + std::io::Seek>(
    archive: &mut ZipArchive<R>,
) -> Result<HashSet<String>, LoaderError> {
    let mut names = HashSet::new();
    for index in 0..archive.len() {
        let entry = archive
            .by_index(index)
            .map_err(|error| legacy_archive_error("Forge", error))?;
        if legacy_archive_entry_is_skipped(entry.name()) {
            continue;
        }
        names.insert(entry.name().to_string());
    }
    Ok(names)
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

#[cfg(test)]
async fn promote_cached_artifact_tmp(tmp_path: &Path, path: &Path) -> Result<(), LoaderError> {
    crate::download::promote_launcher_managed_artifact_temp_once(tmp_path, path)
        .await
        .map_err(LoaderError::Io)
}

async fn write_cached_artifact(
    path: &Path,
    bytes: &[u8],
) -> Result<ExecutionDownloadReport, LoaderError> {
    let tmp_path = artifact_tmp_path(path);
    write_launcher_managed_artifact_bytes_to_temp(path, &tmp_path, bytes)
        .await
        .map_err(loader_execution_download_error)
}

fn loader_execution_download_error(error: ExecutionDownloadError) -> LoaderError {
    loader_download_error(error.into_download_error())
}

fn loader_download_error(error: DownloadError) -> LoaderError {
    match error {
        DownloadError::FileOperation(error) => LoaderError::Io(error),
        error => LoaderError::InstallExecutionFailed(error.to_string()),
    }
}

async fn path_is_file(path: &Path) -> bool {
    matches!(async_fs::metadata(path).await, Ok(metadata) if metadata.is_file())
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

fn cached_installer_path(library_dir: &Path, record: &LoaderBuildRecord) -> PathBuf {
    loader_artifacts_dir(library_dir)
        .join(record.component_id.short_key())
        .join(&record.minecraft_version)
        .join(format!("{}-installer.jar", record.loader_version))
}

fn cached_profile_path(library_dir: &Path, record: &LoaderBuildRecord) -> PathBuf {
    loader_artifacts_dir(library_dir)
        .join(record.component_id.short_key())
        .join(&record.minecraft_version)
        .join(format!("{}-profile.json", record.loader_version))
}

fn cached_legacy_archive_path(library_dir: &Path, record: &LoaderBuildRecord) -> PathBuf {
    let suffix = legacy_archive_cache_suffix(record);
    loader_artifacts_dir(library_dir)
        .join(record.component_id.short_key())
        .join(&record.minecraft_version)
        .join(format!("{}-{suffix}", record.loader_version))
}

fn legacy_archive_cache_suffix(record: &LoaderBuildRecord) -> &'static str {
    match &record.install_source {
        crate::loaders::types::LoaderInstallSource::LegacyArchive { url }
            if url
                .rsplit('/')
                .next()
                .is_some_and(|name| name.ends_with("-universal.zip")) =>
        {
            "universal.zip"
        }
        _ => "client.zip",
    }
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
        ensure_base_version, install_from_installer_source, install_from_legacy_archive,
        install_from_profile_source, is_base_game_installed, promote_cached_artifact_tmp,
        read_cached_artifact, read_valid_installer, read_valid_legacy_archive,
        read_valid_profile_json, strip_child_client_jar_meta, write_cached_artifact,
        write_patched_client_jar_integrity,
    };
    use crate::download::{
        DownloadProgress, ExecutionDownloadFactKind, SelectedDownloadArtifactKind,
    };
    use crate::launch::{Library, LibraryArtifact, LibraryDownload};
    use crate::loaders::types::LoaderError;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability,
    };
    use crate::loaders::validate_version_id;
    use crate::paths::versions_dir;
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
    async fn cached_artifact_write_uses_temp_file_then_rename() {
        let root = temp_dir("cached-artifact-write");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");

        let report = write_cached_artifact(&path, b"installer bytes")
            .await
            .expect("write cached artifact");

        assert_eq!(report.bytes_written, b"installer bytes".len() as u64);
        assert_eq!(report.target, "installer.jar");
        assert!(report.facts.iter().any(|fact| {
            fact.kind == ExecutionDownloadFactKind::MetadataMissing
                && fact
                    .fields
                    .iter()
                    .any(|(key, value)| key == "field" && value == "sha1")
        }));
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionDownloadFactKind::WrittenToTemp)
        );
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionDownloadFactKind::Promoted)
        );
        assert_eq!(fs::read(&path).expect("read artifact"), b"installer bytes");
        assert!(fs::read_dir(&root).expect("read root").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn base_game_installed_requires_selected_libraries() {
        let root = temp_dir("base-installed-requires-libraries");
        let version_id = "1.16";
        let library_bytes = b"library jar";
        write_base_version_with_library(&root, version_id, library_bytes);

        assert!(
            !is_base_game_installed(&root, version_id).await,
            "base install must not be considered complete while selected libraries are missing"
        );

        let library_path = root
            .join("libraries")
            .join("com/example/base/1.0.0/base-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent"))
            .expect("create library parent");
        fs::write(&library_path, library_bytes).expect("write library");

        assert!(is_base_game_installed(&root, version_id).await);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn base_game_installed_rejects_corrupt_selected_libraries() {
        let root = temp_dir("base-installed-rejects-corrupt-libraries");
        let version_id = "1.16";
        write_base_version_with_library(&root, version_id, b"library jar");

        let library_path = root
            .join("libraries")
            .join("com/example/base/1.0.0/base-1.0.0.jar");
        fs::create_dir_all(library_path.parent().expect("library parent"))
            .expect("create library parent");
        fs::write(&library_path, b"wrong").expect("write corrupt library");

        assert!(!is_base_game_installed(&root, version_id).await);
        let _ = fs::remove_dir_all(root);
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

    #[tokio::test]
    async fn cached_artifact_write_replaces_existing_file() {
        let root = temp_dir("cached-artifact-replace");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        fs::write(&path, b"stale bytes").expect("stale artifact");

        write_cached_artifact(&path, b"fresh bytes")
            .await
            .expect("replace cached artifact");

        assert_eq!(fs::read(&path).expect("read artifact"), b"fresh bytes");
        assert!(fs::read_dir(&root).expect("read root").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cached_artifact_promotion_preserves_destination_when_temp_missing() {
        let root = temp_dir("cached-artifact-missing-temp");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        let tmp_path = root.join("installer.tmp");
        fs::write(&path, b"existing bytes").expect("existing artifact");

        let result = promote_cached_artifact_tmp(&tmp_path, &path).await;

        assert!(result.is_err());
        assert_eq!(fs::read(&path).expect("read artifact"), b"existing bytes");
        assert!(!tmp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cached_artifact_write_cleans_temp_file_on_rename_failure() {
        let root = temp_dir("cached-artifact-write-failure");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        fs::create_dir_all(&path).expect("directory at destination");

        let result = write_cached_artifact(&path, b"installer bytes").await;

        assert!(result.is_err());
        assert!(path.is_dir());
        assert!(fs::read_dir(&root).expect("read root").all(|entry| {
            !entry
                .expect("entry")
                .file_name()
                .to_string_lossy()
                .contains(".tmp-")
        }));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_cached_artifact_blocks_without_mutation() {
        let root = temp_dir("cached-artifact-oversized");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        write_oversized_cached_file(&path);

        let error = read_cached_artifact(&path)
            .await
            .expect_err("oversized cached artifact should block");

        assert!(matches!(error, LoaderError::Verify(_)));
        assert!(path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn corrupt_cached_installer_blocks_without_provider_or_mutation() {
        let root = temp_dir("installer-cache-corrupt");
        let path = root.join("artifacts/forge/1.21.5/55.0.0-installer.jar");
        fs::create_dir_all(path.parent().expect("installer parent")).expect("installer parent");
        fs::write(&path, b"corrupt installer").expect("corrupt cached installer");
        let fresh_installer = installer_jar("fresh-installer");
        let server = TestByteServer::start(fresh_installer.clone());

        let error = read_valid_installer(&path, &server.url, "Forge")
            .await
            .expect_err("corrupt cached installer should block");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert_eq!(
            fs::read(&path).expect("cached corrupt installer"),
            b"corrupt installer"
        );
        assert_eq!(server.request_count(), 0);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_invalid_installer_is_not_cached() {
        let root = temp_dir("installer-cache-invalid-fresh");
        let path = root.join("artifacts/forge/1.21.5/55.0.0-installer.jar");
        let server = TestByteServer::start(b"not a zip".to_vec());

        let error = read_valid_installer(&path, &server.url, "Forge")
            .await
            .expect_err("invalid provider installer");

        match error {
            LoaderError::InvalidProfile(message) => {
                assert!(
                    message.starts_with("extracting Forge installer: "),
                    "{message}"
                );
                assert!(!message.contains(&server.url), "{message}");
            }
            error => panic!("expected invalid profile error, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
        assert!(!path.exists());

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn cached_valid_profile_is_used_when_provider_is_unavailable() {
        let root = temp_dir("profile-cache-offline");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(path.parent().expect("profile parent")).expect("profile parent");
        fs::write(&path, profile_json("cached-profile")).expect("cached profile");

        let profile = read_valid_profile_json(&path, "http://127.0.0.1:9/profile/json", "Fabric")
            .await
            .expect("cached profile");

        assert_eq!(profile.fragment.id, "cached-profile");
        assert_eq!(profile.bytes, profile_json("cached-profile"));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_cached_profile_blocks_without_provider_or_mutation() {
        let root = temp_dir("profile-cache-oversized");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(path.parent().expect("profile parent")).expect("profile parent");
        write_oversized_cached_file(&path);
        let fresh = profile_json("fresh-profile");
        let server = TestByteServer::start(fresh.clone());

        let error = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect_err("oversized cached profile should block");

        assert!(matches!(error, LoaderError::Verify(_)));
        assert!(path.exists());
        assert_eq!(server.request_count(), 0);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn corrupt_cached_profile_blocks_without_provider_or_mutation() {
        let root = temp_dir("profile-cache-corrupt");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(path.parent().expect("profile parent")).expect("profile parent");
        fs::write(&path, b"{not-json").expect("corrupt cached profile");
        let fresh = profile_json("fresh-profile");
        let server = TestByteServer::start(fresh.clone());

        let error = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect_err("corrupt cached profile should block");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert_eq!(
            fs::read(&path).expect("cached corrupt profile"),
            b"{not-json"
        );
        assert_eq!(server.request_count(), 0);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_invalid_profile_is_not_cached() {
        let root = temp_dir("profile-cache-invalid-fresh");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        let server = TestByteServer::start(b"{not-json".to_vec());

        let error = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect_err("invalid provider profile");

        match error {
            LoaderError::InvalidProfile(message) => {
                assert!(message.starts_with("Fabric profile: "), "{message}");
                assert!(!message.contains(&server.url), "{message}");
            }
            error => panic!("expected invalid profile error, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
        assert!(!path.exists());

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_valid_profile_survives_cache_write_failure() {
        let root = temp_dir("profile-cache-write-failure");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(&path).expect("blocking profile cache directory");
        let fresh = profile_json("fresh-profile");
        let server = TestByteServer::start(fresh.clone());

        let profile = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect("fresh profile should win over cache persistence failure");

        assert_eq!(profile.fragment.id, "fresh-profile");
        assert_eq!(profile.bytes, fresh);
        assert!(path.is_dir());
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn cached_profile_path_is_component_and_version_scoped() {
        let root = PathBuf::from("/library");
        let path = super::cached_profile_path(&root, &profile_record());

        assert_eq!(
            path,
            root.join("cache")
                .join("loaders")
                .join("artifacts")
                .join("fabric")
                .join("1.21.5")
                .join("0.16.14-profile.json")
        );
    }

    #[test]
    fn cached_legacy_archive_path_tracks_archive_flavor() {
        let root = PathBuf::from("/library");
        let mut record = legacy_archive_record();

        assert_eq!(
            super::cached_legacy_archive_path(&root, &record),
            root.join("cache")
                .join("loaders")
                .join("artifacts")
                .join("forge")
                .join("1.2.5")
                .join("3.4.9.171-client.zip")
        );

        record.minecraft_version = "1.4.7".to_string();
        record.loader_version = "6.6.2.534".to_string();
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: "https://maven.minecraftforge.net/net/minecraftforge/forge/1.4.7-6.6.2.534/forge-1.4.7-6.6.2.534-universal.zip".to_string(),
        };

        assert_eq!(
            super::cached_legacy_archive_path(&root, &record),
            root.join("cache")
                .join("loaders")
                .join("artifacts")
                .join("forge")
                .join("1.4.7")
                .join("6.6.2.534-universal.zip")
        );
    }

    #[tokio::test]
    async fn corrupt_cached_legacy_archive_blocks_without_provider_or_mutation() {
        let root = temp_dir("legacy-archive-corrupt-cache");
        let path = root.join("artifacts/forge/1.2.4/2.0.0.68-client.zip");
        fs::create_dir_all(path.parent().expect("parent")).expect("artifact parent");
        fs::write(&path, b"corrupt cached archive").expect("cached archive");
        let fresh_archive = empty_zip();
        let server = TestByteServer::start(fresh_archive.clone());

        let error = read_valid_legacy_archive(&path, &server.url, "Forge")
            .await
            .expect_err("corrupt cached legacy archive should block");

        assert!(matches!(error, LoaderError::InvalidProfile(_)));
        assert_eq!(
            fs::read(&path).expect("cached corrupt archive"),
            b"corrupt cached archive"
        );
        assert_eq!(server.request_count(), 0);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_cached_legacy_archive_blocks_without_provider_or_mutation() {
        let root = temp_dir("legacy-archive-oversized-cache");
        let path = root.join("artifacts/forge/1.2.4/2.0.0.68-client.zip");
        fs::create_dir_all(path.parent().expect("parent")).expect("artifact parent");
        write_oversized_cached_file(&path);
        let fresh_archive = empty_zip();
        let server = TestByteServer::start(fresh_archive.clone());

        let error = read_valid_legacy_archive(&path, &server.url, "Forge")
            .await
            .expect_err("oversized cached legacy archive should block");

        assert!(matches!(error, LoaderError::Verify(_)));
        assert!(path.exists());
        assert_eq!(server.request_count(), 0);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn fresh_invalid_legacy_archive_returns_bounded_error() {
        let root = temp_dir("legacy-archive-invalid-fresh");
        let path = root.join("artifacts/forge/1.2.4/2.0.0.68-client.zip");
        let server = TestByteServer::start(b"invalid provider archive".to_vec());

        let error = read_valid_legacy_archive(&path, &server.url, "Forge")
            .await
            .expect_err("invalid archive");

        match error {
            LoaderError::InvalidProfile(message) => {
                assert!(
                    message.starts_with("validating Forge legacy archive: "),
                    "{message}"
                );
                assert!(!message.contains(&server.url), "{message}");
            }
            error => panic!("expected invalid profile error, got {error:?}"),
        }
        assert_eq!(server.request_count(), 1);
        assert!(!path.exists());

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
    async fn profile_source_installs_to_backend_version_id_when_upstream_id_differs() {
        let root = temp_dir("profile-upstream-id-mismatch");
        write_base_version(&root, "1.21.5");
        let mut record = profile_record();
        record.version_id = "backend-profile-id".to_string();
        let profile_path = super::cached_profile_path(&root, &record);
        fs::create_dir_all(profile_path.parent().expect("profile parent"))
            .expect("create profile cache parent");
        fs::write(&profile_path, profile_json("upstream-profile-id")).expect("cached profile");
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let mut progress = |_progress: DownloadProgress| {};

        let installed_version_id = install_from_profile_source(
            &root,
            &plan,
            "http://127.0.0.1:9/profile/json",
            &mut progress,
        )
        .await
        .expect("install profile-backed loader");

        assert_eq!(installed_version_id, record.version_id);
        assert_backend_version_was_written(&root, &record.version_id, "upstream-profile-id");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn profile_source_marks_checksumless_libraries_in_composed_version() {
        let root = temp_dir("profile-marks-checksumless-libraries");
        write_base_version(&root, "1.21.5");
        let library_body = zip_entries(&[("org/quiltmc/loader/impl/QuiltLoader.class", b"loader")]);
        let server = TestByteServer::start(library_body);
        let record = profile_record();
        let profile_path = super::cached_profile_path(&root, &record);
        fs::create_dir_all(profile_path.parent().expect("profile parent"))
            .expect("create profile cache parent");
        fs::write(
            &profile_path,
            profile_json_with_checksumless_library("upstream-profile-id", &server.url),
        )
        .expect("cached profile");
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let mut progress = |_progress: DownloadProgress| {};

        install_from_profile_source(
            &root,
            &plan,
            "http://127.0.0.1:9/profile/json",
            &mut progress,
        )
        .await
        .expect("install profile-backed loader");

        let version_json = fs::read(
            versions_dir(&root)
                .join(&record.version_id)
                .join(format!("{}.json", record.version_id)),
        )
        .expect("read composed profile");
        let version: serde_json::Value =
            serde_json::from_slice(&version_json).expect("parse composed profile");
        let libraries = version["libraries"].as_array().expect("libraries");
        assert!(libraries.iter().any(|library| {
            library["name"] == "org.quiltmc:quilt-loader:0.29.2"
                && library["axialChecksumlessAllowed"] == true
        }));

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installer_source_installs_to_backend_version_id_when_upstream_id_differs() {
        let root = temp_dir("installer-upstream-id-mismatch");
        write_base_version(&root, "1.21.5");
        let mut record = installer_record();
        record.version_id = "backend-installer-id".to_string();
        let installer_path = super::cached_installer_path(&root, &record);
        fs::create_dir_all(installer_path.parent().expect("installer parent"))
            .expect("create installer cache parent");
        fs::write(&installer_path, installer_jar("upstream-installer-id"))
            .expect("cached installer");
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };
        let mut progress = |_progress: DownloadProgress| {};

        let installed_version_id = install_from_installer_source(
            &root,
            &plan,
            "http://127.0.0.1:9/installer.jar",
            &mut progress,
        )
        .await
        .expect("install installer-backed loader");

        assert_eq!(installed_version_id, record.version_id);
        assert_backend_version_was_written(&root, &record.version_id, "upstream-installer-id");
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn installer_source_allows_checksumless_legacy_profile_libraries() {
        let root = temp_dir("installer-checksumless-legacy-libraries");
        let minecraft_version = "1.7.10";
        write_base_version(&root, minecraft_version);
        let library_body = zip_entries(&[("net/minecraftforge/Forge.class", b"forge")]);
        let server = TestByteServer::start(library_body);
        let mut record = installer_record();
        record.minecraft_version = minecraft_version.to_string();
        record.loader_version = "10.13.4.1614-1.7.10".to_string();
        record.version_id = "1.7.10-forge-10.13.4.1614-1.7.10".to_string();
        let installer_path = super::cached_installer_path(&root, &record);
        fs::create_dir_all(installer_path.parent().expect("installer parent"))
            .expect("create installer cache parent");
        fs::write(
            &installer_path,
            installer_jar_with_profile_json(
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
                    server.url
                )
                .as_bytes(),
            ),
        )
        .expect("cached installer");
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };

        let installed_version_id = install_from_installer_source(
            &root,
            &plan,
            "http://127.0.0.1:9/installer.jar",
            &mut |_| {},
        )
        .await
        .expect("install legacy installer-backed loader");

        assert_eq!(installed_version_id, record.version_id);
        assert_eq!(server.request_count(), 1);
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
                && library["axialChecksumlessAllowed"] == true
        }));

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn strip_meta_legacy_installer_rewrites_child_client_and_integrity() {
        let root = temp_dir("installer-strip-meta-child-client");
        let version_id = "1.5.2-forge-7.8.1.738";
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
                "id":"1.5.2-forge-7.8.1.738",
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
        let version_id = "1.5.2-forge-7.8.1.738";
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
        record.loader_version = "7.8.1.738".to_string();
        record.version_id = version_id.to_string();
        record.strategy = LoaderInstallStrategy::ForgeLegacyInstaller;
        let installer_path = super::cached_installer_path(&root, &record);
        fs::create_dir_all(installer_path.parent().expect("installer parent"))
            .expect("create installer cache parent");
        fs::write(&installer_path, installer).expect("write cached installer");
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };

        install_from_installer_source(
            &root,
            &plan,
            "http://127.0.0.1:9/installer.jar",
            &mut |_| {},
        )
        .await
        .expect("install stripMeta legacy installer");

        let child_jar = versions_dir(&root)
            .join(version_id)
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
                .join(version_id)
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
        let server = TestByteServer::start(forge_archive);
        let mut record = legacy_archive_record();
        record.minecraft_version = base_version_id.to_string();
        record.version_id = "forge-1.2.5-3.4.9.171".to_string();
        let plan = LoaderInstallPlan {
            record: record.clone(),
            stage_dir: root.join("stage"),
        };

        let installed_version_id =
            install_from_legacy_archive(&root, &plan, &server.url, &mut |_progress| {})
                .await
                .expect("install legacy archive");

        assert_eq!(installed_version_id, record.version_id);
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

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("axial-{prefix}-{nanos:x}"))
    }

    fn write_oversized_cached_file(path: &std::path::Path) {
        fs::File::create(path)
            .expect("oversized cached file")
            .set_len(super::MAX_INSTALLER_DOWNLOAD_SIZE + 1)
            .expect("size oversized cached file");
    }

    fn empty_zip() -> Vec<u8> {
        zip_entries(&[])
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

    fn profile_json_with_checksumless_library(id: &str, library_url: &str) -> Vec<u8> {
        format!(
            r#"{{
                "id":"{id}",
                "mainClass":"org.quiltmc.loader.impl.launch.knot.KnotClient",
                "libraries":[{{
                    "name":"org.quiltmc:quilt-loader:0.29.2",
                    "url":"{library_url}"
                }}]
            }}"#
        )
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

    fn write_base_version_with_library(
        root: &std::path::Path,
        version_id: &str,
        library_bytes: &[u8],
    ) {
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
                    "libraries":[{{
                        "name":"com.example:base:1.0.0",
                        "downloads":{{
                            "artifact":{{
                                "path":"com/example/base/1.0.0/base-1.0.0.jar",
                                "url":"https://example.invalid/base-1.0.0.jar",
                                "sha1":"{}",
                                "size":{}
                            }}
                        }}
                    }}]
                }}"#,
                sha1_hex(library_bytes),
                library_bytes.len()
            ),
        )
        .expect("write base version json");
        fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
            .expect("write base jar");
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
        LoaderBuildRecord {
            subject_kind: LoaderBuildSubjectKind::LoaderBuild,
            component_id: LoaderComponentId::Fabric,
            component_name: "Fabric".to_string(),
            build_id: "fabric-1.21.5-0.16.14".to_string(),
            minecraft_version: "1.21.5".to_string(),
            loader_version: "0.16.14".to_string(),
            version_id: "fabric-loader-0.16.14-1.21.5".to_string(),
            build_meta: LoaderBuildMetadata::default(),
            strategy: LoaderInstallStrategy::FabricProfile,
            artifact_kind: LoaderArtifactKind::ProfileJson,
            installability: LoaderInstallability::Installable,
            install_source: LoaderInstallSource::ProfileJson {
                url: "https://meta.fabricmc.net/profile/json".to_string(),
            },
        }
    }

    fn installer_record() -> LoaderBuildRecord {
        let mut record = profile_record();
        record.component_id = LoaderComponentId::Forge;
        record.component_name = "Forge".to_string();
        record.build_id = "forge-1.21.5-55.0.0".to_string();
        record.loader_version = "55.0.0".to_string();
        record.version_id = "forge-1.21.5-55.0.0".to_string();
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
        record.build_id = "forge-1.2.5-3.4.9.171".to_string();
        record.minecraft_version = "1.2.5".to_string();
        record.loader_version = "3.4.9.171".to_string();
        record.version_id = "forge-1.2.5-3.4.9.171".to_string();
        record.strategy = LoaderInstallStrategy::ForgeEarliestLegacy;
        record.artifact_kind = LoaderArtifactKind::LegacyArchive;
        record.install_source = LoaderInstallSource::LegacyArchive {
            url: "https://example.test/legacy.jar".to_string(),
        };
        record
    }

    struct TestByteServer {
        url: String,
        request_count: Arc<AtomicUsize>,
        stop_server: mpsc::Sender<()>,
        server: thread::JoinHandle<()>,
    }

    impl TestByteServer {
        fn start(body: Vec<u8>) -> Self {
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
                            respond_ok(stream, &body);
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

    fn respond_ok(mut stream: TcpStream, body: &[u8]) {
        let mut buffer = [0_u8; 1024];
        let _ = stream.read(&mut buffer);
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
