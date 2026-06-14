use crate::download::{DownloadProgress, Downloader, download_libraries};
use crate::launch::resolve_version;
use crate::loaders::compose::{
    LoaderProfileFragment, cleanup_incomplete_version, compose_loader_version,
    finalize_version_install, write_composed_version,
};
use crate::loaders::forge_installer::{
    ExtractedForgeInstaller, ForgeInstallerError, extract_installer, extract_maven_entries,
};
use crate::loaders::http::fetch_bytes;
use crate::loaders::processors::run_processors;
use crate::loaders::types::{LoaderBuildRecord, LoaderError, LoaderInstallPlan};
use crate::loaders::validate_version_id;
use crate::paths::{loader_artifacts_dir, versions_dir};
use crate::profiles::ensure_launcher_profiles;
use std::collections::HashMap;
use std::io::sink;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;
use zip::ZipArchive;
use zip::result::ZipError;

const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;

#[derive(Debug)]
struct CachedArtifact {
    bytes: Vec<u8>,
    cache_hit: bool,
}

#[derive(Debug)]
struct CachedProfile {
    bytes: Vec<u8>,
    fragment: LoaderProfileFragment,
}

#[derive(Debug)]
enum InstallerTaskError {
    Extract(ForgeInstallerError),
    Task(tokio::task::JoinError),
}

impl std::fmt::Display for InstallerTaskError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Extract(error) => write!(formatter, "{error}"),
            Self::Task(error) => write!(formatter, "blocking task failed: {error}"),
        }
    }
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
    let fragment = cached_profile.fragment;
    let installed_version_id = if fragment.id.trim().is_empty() {
        plan.record.version_id.clone()
    } else {
        fragment.id.clone()
    };
    validate_version_id(&installed_version_id, "installed loader version id")?;

    cleanup_on_error(
        write_raw_profile_version(library_dir, &installed_version_id, &profile_bytes).await,
        library_dir,
        &installed_version_id,
    )?;
    let library_download_result = Box::pin(download_libraries(
        library_dir,
        &fragment.libraries,
        "loader_libraries",
        &mut *send,
    ))
    .await
    .map_err(|error| LoaderError::Other(format!("downloading loader libraries: {error}")));
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
    let cached_installer = read_or_download_cached_artifact(&installer_path, installer_url).await?;

    send(progress(
        "profile",
        0,
        1,
        Some(format!(
            "Extracting {} installer...",
            plan.record.component_name
        )),
    ));
    let (installer_data, extracted) = match extract_installer_blocking(cached_installer.bytes).await
    {
        Ok(extracted) => extracted,
        Err(InstallerTaskError::Extract(_)) if cached_installer.cache_hit => {
            let _ = async_fs::remove_file(&installer_path).await;
            let bytes = download_and_cache_artifact(installer_url, &installer_path).await?;
            extract_installer_blocking(bytes)
                .await
                .map_err(|fresh_error| {
                    installer_task_extract_error(&plan.record.component_name, fresh_error)
                })?
        }
        Err(error) => {
            return Err(installer_task_extract_error(
                &plan.record.component_name,
                error,
            ));
        }
    };
    let installed_version_id = if extracted.version_id.trim().is_empty() {
        plan.record.version_id.clone()
    } else {
        extracted.version_id.clone()
    };
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
        extract_maven_entries_blocking(installer_data, library_dir.to_path_buf())
            .await
            .map_err(|error| {
                LoaderError::Other(format!(
                    "extracting {} installer libraries: {error}",
                    plan.record.component_name
                ))
            }),
        library_dir,
        &installed_version_id,
    )?;
    let library_download_result = Box::pin(download_libraries(
        library_dir,
        &extracted.libraries,
        "loader_libraries",
        &mut *send,
    ))
    .await
    .map_err(|error| {
        LoaderError::Other(format!(
            "downloading {} libraries: {error}",
            plan.record.component_name
        ))
    });
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
                });
            },
        ))
        .await
        .map_err(|error| {
            LoaderError::Other(format!(
                "running {} processors: {error}",
                plan.record.component_name
            ))
        });
        cleanup_on_error(processor_result, library_dir, &installed_version_id)?;
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

    let version_jar = versions_dir(library_dir)
        .join(&plan.record.version_id)
        .join(format!("{}.jar", plan.record.version_id));
    cleanup_on_error(
        async_fs::write(&version_jar, archive_data).await,
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
    if is_base_game_installed(library_dir, version_id) {
        return Ok(());
    }

    let install_lock = base_version_install_lock(library_dir, version_id);
    let _guard = install_lock.lock().await;
    if is_base_game_installed(library_dir, version_id) {
        return Ok(());
    }

    let downloader = Downloader::new(library_dir.to_path_buf());
    Box::pin(downloader.install_version(version_id, None, |progress| {
        if !progress.done {
            send(progress);
        }
    }))
    .await?;
    Ok(())
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

fn is_base_game_installed(library_dir: &Path, game_version: &str) -> bool {
    let version_dir = versions_dir(library_dir).join(game_version);
    let json_path = version_dir.join(format!("{game_version}.json"));
    let jar_path = version_dir.join(format!("{game_version}.jar"));
    let marker_path = version_dir.join(".incomplete");
    json_path.is_file() && jar_path.is_file() && !marker_path.exists()
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
                let _ = write_cached_artifact(path, &bytes).await;
                return Ok(CachedProfile { bytes, fragment });
            }
        };
        match parse_profile_json(&bytes, component_name) {
            Ok(fragment) => return Ok(CachedProfile { bytes, fragment }),
            Err(_) => {
                let _ = async_fs::remove_file(path).await;
            }
        }
    }

    let bytes = download_to_memory(url).await?;
    let fragment = parse_profile_json(&bytes, component_name)?;
    let _ = write_cached_artifact(path, &bytes).await;
    Ok(CachedProfile { bytes, fragment })
}

fn parse_profile_json(
    bytes: &[u8],
    component_name: &str,
) -> Result<LoaderProfileFragment, LoaderError> {
    serde_json::from_slice::<LoaderProfileFragment>(bytes)
        .map_err(|error| LoaderError::InvalidProfile(format!("{component_name} profile: {error}")))
}

async fn read_or_download_cached_artifact(
    path: &Path,
    url: &str,
) -> Result<CachedArtifact, LoaderError> {
    if path_is_file(path).await
        && let Some(bytes) = read_cached_artifact(path).await?
    {
        return Ok(CachedArtifact {
            bytes,
            cache_hit: true,
        });
    }

    Ok(CachedArtifact {
        bytes: download_and_cache_artifact(url, path).await?,
        cache_hit: false,
    })
}

async fn download_and_cache_artifact(url: &str, path: &Path) -> Result<Vec<u8>, LoaderError> {
    let bytes = download_to_memory(url).await?;
    write_cached_artifact(path, &bytes).await?;
    Ok(bytes)
}

async fn read_valid_legacy_archive(
    path: &Path,
    url: &str,
    component_name: &str,
) -> Result<Vec<u8>, LoaderError> {
    let cached_archive = read_or_download_cached_artifact(path, url).await?;
    match validate_legacy_archive(&cached_archive.bytes) {
        Ok(()) => Ok(cached_archive.bytes),
        Err(_) if cached_archive.cache_hit => {
            let _ = async_fs::remove_file(path).await;
            let bytes = download_and_cache_artifact(url, path).await?;
            if let Err(error) = validate_legacy_archive(&bytes) {
                let _ = async_fs::remove_file(path).await;
                return Err(legacy_archive_error(component_name, error));
            }
            Ok(bytes)
        }
        Err(error) => {
            let _ = async_fs::remove_file(path).await;
            Err(legacy_archive_error(component_name, error))
        }
    }
}

async fn read_cached_artifact(path: &Path) -> Result<Option<Vec<u8>>, LoaderError> {
    let metadata = async_fs::metadata(path).await?;
    if metadata.len() > MAX_INSTALLER_DOWNLOAD_SIZE {
        let _ = async_fs::remove_file(path).await;
        return Ok(None);
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
) -> Result<(Vec<u8>, ExtractedForgeInstaller), InstallerTaskError> {
    tokio::task::spawn_blocking(move || {
        let extracted = extract_installer(&installer_data).map_err(InstallerTaskError::Extract)?;
        Ok((installer_data, extracted))
    })
    .await
    .map_err(InstallerTaskError::Task)?
}

async fn extract_maven_entries_blocking(
    installer_data: Vec<u8>,
    library_dir: PathBuf,
) -> Result<Vec<u8>, InstallerTaskError> {
    tokio::task::spawn_blocking(move || {
        extract_maven_entries(&installer_data, &library_dir)
            .map_err(InstallerTaskError::Extract)?;
        Ok(installer_data)
    })
    .await
    .map_err(InstallerTaskError::Task)?
}

async fn promote_cached_artifact_tmp(tmp_path: &Path, path: &Path) -> Result<(), LoaderError> {
    let first_error = match async_fs::rename(tmp_path, path).await {
        Ok(()) => return Ok(()),
        Err(error) => error,
    };

    match async_fs::symlink_metadata(tmp_path).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(first_error.into());
        }
        Err(error) => return Err(error.into()),
    }

    match async_fs::symlink_metadata(path).await {
        Ok(metadata) if metadata.file_type().is_dir() => return Err(first_error.into()),
        Ok(metadata) if metadata.file_type().is_file() || metadata.file_type().is_symlink() => {
            async_fs::remove_file(path).await?;
        }
        Ok(_) => return Err(first_error.into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }

    match async_fs::rename(tmp_path, path).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = async_fs::remove_file(tmp_path).await;
            Err(error.into())
        }
    }
}

async fn write_cached_artifact(path: &Path, bytes: &[u8]) -> Result<(), LoaderError> {
    if let Some(parent) = path.parent() {
        async_fs::create_dir_all(parent).await?;
    }

    let tmp_path = artifact_tmp_path(path);
    let result = async {
        async_fs::write(&tmp_path, bytes).await?;
        promote_cached_artifact_tmp(&tmp_path, path).await?;
        Ok::<_, LoaderError>(())
    }
    .await;
    if result.is_err() {
        let _ = async_fs::remove_file(&tmp_path).await;
    }
    result
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

fn installer_task_extract_error(component_name: &str, error: InstallerTaskError) -> LoaderError {
    match error {
        InstallerTaskError::Extract(error) => installer_extract_error(component_name, error),
        InstallerTaskError::Task(error) => LoaderError::Other(format!(
            "extracting {component_name} installer: blocking task failed: {error}"
        )),
    }
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
    loader_artifacts_dir(library_dir)
        .join(record.component_id.short_key())
        .join(&record.minecraft_version)
        .join(format!("{}-client.zip", record.loader_version))
}

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
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
    }
}

#[cfg(test)]
mod tests {
    use super::{
        base_version_install_lock_from_map, cleanup_on_error, ensure_base_version,
        install_from_installer_source, install_from_legacy_archive, install_from_profile_source,
        promote_cached_artifact_tmp, read_or_download_cached_artifact, read_valid_legacy_archive,
        read_valid_profile_json, write_cached_artifact,
    };
    use crate::download::DownloadProgress;
    use crate::loaders::types::LoaderError;
    use crate::loaders::types::{
        LoaderArtifactKind, LoaderBuildMetadata, LoaderBuildRecord, LoaderBuildSubjectKind,
        LoaderComponentId, LoaderInstallPlan, LoaderInstallSource, LoaderInstallStrategy,
        LoaderInstallability,
    };
    use crate::loaders::validate_version_id;
    use crate::paths::versions_dir;
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
        let root = PathBuf::from("/tmp/croopor-loader-future-size");
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
        let root = PathBuf::from("/tmp/croopor-loader-base-lock");
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
        let root = PathBuf::from("/tmp/croopor-loader-poisoned-base-lock");
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

        write_cached_artifact(&path, b"installer bytes")
            .await
            .expect("write cached artifact");

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
    async fn oversized_cached_artifact_is_removed_and_replaced_from_provider() {
        let root = temp_dir("cached-artifact-oversized");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        write_oversized_cached_file(&path);
        let fresh = b"fresh installer bytes".to_vec();
        let server = TestByteServer::start(fresh.clone());

        let artifact = read_or_download_cached_artifact(&path, &server.url)
            .await
            .expect("fresh artifact");

        assert!(!artifact.cache_hit);
        assert_eq!(artifact.bytes, fresh);
        assert_eq!(fs::read(&path).expect("cached fresh artifact"), fresh);
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_cached_artifact_is_removed_before_provider_failure() {
        let root = temp_dir("cached-artifact-oversized-offline");
        fs::create_dir_all(&root).expect("root");
        let path = root.join("installer.jar");
        write_oversized_cached_file(&path);

        let error = read_or_download_cached_artifact(&path, "http://127.0.0.1:9/installer.jar")
            .await
            .expect_err("provider failure");

        assert!(error.to_string().contains("request failed"));
        assert!(!path.exists());

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
    async fn oversized_cached_profile_is_removed_and_replaced_from_provider() {
        let root = temp_dir("profile-cache-oversized");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(path.parent().expect("profile parent")).expect("profile parent");
        write_oversized_cached_file(&path);
        let fresh = profile_json("fresh-profile");
        let server = TestByteServer::start(fresh.clone());

        let profile = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect("fresh profile");

        assert_eq!(profile.fragment.id, "fresh-profile");
        assert_eq!(profile.bytes, fresh);
        assert_eq!(fs::read(&path).expect("cached fresh profile"), fresh);
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn corrupt_cached_profile_is_removed_and_replaced_from_provider() {
        let root = temp_dir("profile-cache-corrupt");
        let path = root.join("artifacts/fabric/1.21.5/0.16.14-profile.json");
        fs::create_dir_all(path.parent().expect("profile parent")).expect("profile parent");
        fs::write(&path, b"{not-json").expect("corrupt cached profile");
        let fresh = profile_json("fresh-profile");
        let server = TestByteServer::start(fresh.clone());

        let profile = read_valid_profile_json(&path, &server.url, "Fabric")
            .await
            .expect("fresh profile");

        assert_eq!(profile.fragment.id, "fresh-profile");
        assert_eq!(fs::read(&path).expect("cached fresh profile"), fresh);
        assert_eq!(server.request_count(), 1);

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

    #[tokio::test]
    async fn cached_legacy_archive_corruption_fetches_provider_once() {
        let root = temp_dir("legacy-archive-corrupt-cache");
        let path = root.join("artifacts/forge/1.2.4/2.0.0.68-client.zip");
        fs::create_dir_all(path.parent().expect("parent")).expect("artifact parent");
        fs::write(&path, b"corrupt cached archive").expect("cached archive");
        let fresh_archive = empty_zip();
        let server = TestByteServer::start(fresh_archive.clone());

        let bytes = read_valid_legacy_archive(&path, &server.url, "Forge")
            .await
            .expect("legacy archive");

        assert_eq!(bytes, fresh_archive);
        assert_eq!(
            fs::read(&path).expect("cached fresh archive"),
            fresh_archive
        );
        assert_eq!(server.request_count(), 1);

        server.stop();
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn oversized_cached_legacy_archive_is_removed_and_replaced_from_provider() {
        let root = temp_dir("legacy-archive-oversized-cache");
        let path = root.join("artifacts/forge/1.2.4/2.0.0.68-client.zip");
        fs::create_dir_all(path.parent().expect("parent")).expect("artifact parent");
        write_oversized_cached_file(&path);
        let fresh_archive = empty_zip();
        let server = TestByteServer::start(fresh_archive.clone());

        let bytes = read_valid_legacy_archive(&path, &server.url, "Forge")
            .await
            .expect("legacy archive");

        assert_eq!(bytes, fresh_archive);
        assert_eq!(
            fs::read(&path).expect("cached fresh archive"),
            fresh_archive
        );
        assert_eq!(server.request_count(), 1);

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
        assert_eq!(error.to_string(), "installed loader version id is empty");
    }

    #[test]
    fn rejects_whitespace_padded_installed_version_id() {
        let error =
            validate_version_id(" loader-id ", "installed loader version id").expect_err("error");
        assert_eq!(
            error.to_string(),
            "installed loader version id contains surrounding whitespace"
        );
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!("croopor-{prefix}-{nanos:x}"))
    }

    fn write_oversized_cached_file(path: &std::path::Path) {
        fs::File::create(path)
            .expect("oversized cached file")
            .set_len(super::MAX_INSTALLER_DOWNLOAD_SIZE + 1)
            .expect("size oversized cached file");
    }

    fn empty_zip() -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        zip::ZipWriter::new(&mut cursor)
            .finish()
            .expect("finish empty zip");
        cursor.into_inner()
    }

    fn profile_json(id: &str) -> Vec<u8> {
        format!(r#"{{"id":"{id}","mainClass":"net.fabricmc.loader.impl.launch.knot.KnotClient"}}"#)
            .into_bytes()
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
