use crate::download::{DownloadProgress, Downloader, download_libraries};
use crate::launch::resolve_version;
use crate::loaders::compose::{
    LoaderProfileFragment, cleanup_incomplete_version, compose_loader_version,
    finalize_version_install, write_composed_version,
};
use crate::loaders::forge_installer::{extract_installer, extract_maven_entries};
use crate::loaders::http::fetch_bytes;
use crate::loaders::processors::run_processors;
use crate::loaders::types::{LoaderBuildRecord, LoaderError, LoaderInstallPlan};
use crate::loaders::validate_version_id;
use crate::paths::{loader_artifacts_dir, versions_dir};
use crate::profiles::ensure_launcher_profiles;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;

const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;

struct CachedArtifact {
    bytes: Vec<u8>,
    cache_hit: bool,
}

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
    let profile_bytes = download_to_memory(profile_url).await?;
    let fragment = serde_json::from_slice::<LoaderProfileFragment>(&profile_bytes)
        .map_err(|error| LoaderError::InvalidProfile(error.to_string()))?;
    let installed_version_id = if fragment.id.trim().is_empty() {
        plan.record.version_id.clone()
    } else {
        fragment.id.clone()
    };
    validate_version_id(&installed_version_id, "installed loader version id")?;

    cleanup_on_error(
        write_raw_profile_version(library_dir, &installed_version_id, &profile_bytes),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        download_libraries(
            library_dir,
            &fragment.libraries,
            "loader_libraries",
            &mut *send,
        )
        .await
        .map_err(|error| LoaderError::Other(format!("downloading loader libraries: {error}"))),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        ensure_base_version(library_dir, &plan.record.minecraft_version, send).await,
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
        ),
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

pub async fn install_from_installer_source<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    installer_url: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    ensure_base_version(library_dir, &plan.record.minecraft_version, send).await?;
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
    let (installer_data, extracted) = match extract_installer(&cached_installer.bytes) {
        Ok(extracted) => (cached_installer.bytes, extracted),
        Err(error) if cached_installer.cache_hit => {
            let _ = async_fs::remove_file(&installer_path).await;
            let bytes = download_and_cache_artifact(installer_url, &installer_path).await?;
            let extracted = extract_installer(&bytes).map_err(|fresh_error| {
                installer_extract_error(&plan.record.component_name, fresh_error)
            })?;
            (bytes, extracted)
        }
        Err(error) => return Err(installer_extract_error(&plan.record.component_name, error)),
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
        ),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        extract_maven_entries(&installer_data, library_dir).map_err(|error| {
            LoaderError::Other(format!(
                "extracting {} installer libraries: {error}",
                plan.record.component_name
            ))
        }),
        library_dir,
        &installed_version_id,
    )?;
    cleanup_on_error(
        download_libraries(
            library_dir,
            &extracted.libraries,
            "loader_libraries",
            &mut *send,
        )
        .await
        .map_err(|error| {
            LoaderError::Other(format!(
                "downloading {} libraries: {error}",
                plan.record.component_name
            ))
        }),
        library_dir,
        &installed_version_id,
    )?;

    if let Some(install_profile_json) = extracted.install_profile_json.as_deref() {
        send(progress(
            "processors",
            0,
            1,
            Some("Running processors...".to_string()),
        ));
        cleanup_on_error(
            run_processors(
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
            )
            .await
            .map_err(|error| {
                LoaderError::Other(format!(
                    "running {} processors: {error}",
                    plan.record.component_name
                ))
            }),
            library_dir,
            &installed_version_id,
        )?;
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

pub async fn install_from_legacy_archive<F>(
    library_dir: &Path,
    plan: &LoaderInstallPlan,
    archive_url: &str,
    send: &mut F,
) -> Result<String, LoaderError>
where
    F: FnMut(DownloadProgress),
{
    ensure_base_version(library_dir, &plan.record.minecraft_version, send).await?;
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
    let archive_data = read_or_download_cached_artifact(&archive_path, archive_url)
        .await?
        .bytes;

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
        ),
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

    let downloader = Downloader::new(library_dir.to_path_buf());
    downloader
        .install_version(version_id, None, |progress| {
            if !progress.done {
                send(progress);
            }
        })
        .await?;
    Ok(())
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

fn write_raw_profile_version(
    library_dir: &Path,
    version_id: &str,
    profile_bytes: &[u8],
) -> Result<(), LoaderError> {
    validate_version_id(version_id, "installed loader version id")?;
    let version_dir = versions_dir(library_dir).join(version_id);
    fs::create_dir_all(&version_dir)?;
    fs::write(version_dir.join(".incomplete"), b"installing")?;
    fs::write(
        version_dir.join(format!("{version_id}.json")),
        profile_bytes,
    )?;
    Ok(())
}

async fn download_to_memory(url: &str) -> Result<Vec<u8>, LoaderError> {
    fetch_bytes(url, MAX_INSTALLER_DOWNLOAD_SIZE).await
}

async fn read_or_download_cached_artifact(
    path: &Path,
    url: &str,
) -> Result<CachedArtifact, LoaderError> {
    if path_is_file(path).await {
        return Ok(CachedArtifact {
            bytes: async_fs::read(path).await?,
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

fn cached_installer_path(library_dir: &Path, record: &LoaderBuildRecord) -> PathBuf {
    loader_artifacts_dir(library_dir)
        .join(record.component_id.short_key())
        .join(&record.minecraft_version)
        .join(format!("{}-installer.jar", record.loader_version))
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
    use super::{cleanup_on_error, promote_cached_artifact_tmp, write_cached_artifact};
    use crate::loaders::types::LoaderError;
    use crate::loaders::validate_version_id;
    use crate::paths::versions_dir;
    use std::fs;
    use std::path::PathBuf;
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
}
