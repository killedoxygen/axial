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
use crate::paths::{loader_artifacts_dir, versions_dir};
use crate::profiles::ensure_launcher_profiles;
use std::fs;
use std::path::{Path, PathBuf};

const MAX_INSTALLER_DOWNLOAD_SIZE: u64 = 50 << 20;

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

    write_raw_profile_version(library_dir, &installed_version_id, &profile_bytes)?;

    if let Err(error) = download_libraries(
        library_dir,
        &fragment.libraries,
        "loader_libraries",
        |progress| send(progress),
    )
    .await
    {
        cleanup_incomplete_version(library_dir, &installed_version_id);
        return Err(LoaderError::Other(format!(
            "downloading loader libraries: {error}"
        )));
    }

    ensure_base_version(library_dir, &plan.record.minecraft_version, send)
        .await
        .inspect_err(|_| cleanup_incomplete_version(library_dir, &installed_version_id))?;

    let version = compose_loader_version(
        library_dir,
        &plan.record.minecraft_version,
        &installed_version_id,
        &fragment,
    )
    .inspect_err(|_| cleanup_incomplete_version(library_dir, &installed_version_id))?;
    write_composed_version(
        library_dir,
        &installed_version_id,
        &version,
        &plan.record.minecraft_version,
    )
    .inspect_err(|_| cleanup_incomplete_version(library_dir, &installed_version_id))?;
    verify_install(library_dir, &installed_version_id)
        .inspect_err(|_| cleanup_incomplete_version(library_dir, &installed_version_id))?;
    finalize_version_install(library_dir, &installed_version_id)
        .inspect_err(|_| cleanup_incomplete_version(library_dir, &installed_version_id))?;
    ensure_launcher_profiles(library_dir, &installed_version_id)?;
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
    let installer_data = if installer_path.is_file() {
        fs::read(&installer_path)?
    } else {
        let bytes = download_to_memory(installer_url).await?;
        if let Some(parent) = installer_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&installer_path, &bytes)?;
        bytes
    };

    send(progress(
        "profile",
        0,
        1,
        Some(format!(
            "Extracting {} installer...",
            plan.record.component_name
        )),
    ));
    let extracted = extract_installer(&installer_data).map_err(|error| {
        LoaderError::InvalidProfile(format!(
            "extracting {} installer: {error}",
            plan.record.component_name
        ))
    })?;
    let installed_version_id = if extracted.version_id.trim().is_empty() {
        plan.record.version_id.clone()
    } else {
        extracted.version_id.clone()
    };
    let version = compose_loader_version(
        library_dir,
        &plan.record.minecraft_version,
        &installed_version_id,
        &extracted.version_fragment,
    )?;
    write_composed_version(
        library_dir,
        &installed_version_id,
        &version,
        &plan.record.minecraft_version,
    )?;

    if let Err(error) = extract_maven_entries(&installer_data, library_dir) {
        cleanup_incomplete_version(library_dir, &installed_version_id);
        return Err(LoaderError::Other(format!(
            "extracting {} installer libraries: {error}",
            plan.record.component_name
        )));
    }

    if let Err(error) = download_libraries(
        library_dir,
        &extracted.libraries,
        "loader_libraries",
        |progress| send(progress),
    )
    .await
    {
        cleanup_incomplete_version(library_dir, &installed_version_id);
        return Err(LoaderError::Other(format!(
            "downloading {} libraries: {error}",
            plan.record.component_name
        )));
    }

    if let Some(install_profile_json) = extracted.install_profile_json.as_deref() {
        send(progress(
            "processors",
            0,
            1,
            Some("Running processors...".to_string()),
        ));
        if let Err(error) = run_processors(
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
        {
            cleanup_incomplete_version(library_dir, &installed_version_id);
            return Err(LoaderError::Other(format!(
                "running {} processors: {error}",
                plan.record.component_name
            )));
        }
    }

    verify_install(library_dir, &installed_version_id)?;
    finalize_version_install(library_dir, &installed_version_id)?;
    ensure_launcher_profiles(library_dir, &installed_version_id)?;
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
    let archive_data = if archive_path.is_file() {
        fs::read(&archive_path)?
    } else {
        let bytes = download_to_memory(archive_url).await?;
        if let Some(parent) = archive_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&archive_path, &bytes)?;
        bytes
    };

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
    write_composed_version(
        library_dir,
        &plan.record.version_id,
        &version,
        &plan.record.minecraft_version,
    )?;

    let version_jar = versions_dir(library_dir)
        .join(&plan.record.version_id)
        .join(format!("{}.jar", plan.record.version_id));
    fs::write(&version_jar, archive_data)?;

    verify_install(library_dir, &plan.record.version_id)?;
    finalize_version_install(library_dir, &plan.record.version_id)?;
    ensure_launcher_profiles(library_dir, &plan.record.version_id)?;
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

fn verify_install(library_dir: &Path, version_id: &str) -> Result<(), LoaderError> {
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
