use super::file_download::{
    RuntimeDownloadEvidence, bounded_manifest_file_label, component_manifest_destination,
    fetch_runtime_file, runtime_download_client, runtime_download_temp_path,
    runtime_file_download_concurrency,
};
use super::layout::{java_executable, runtime_executable_ready, runtime_os_arch};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, ComponentManifestFile, RUNTIME_MANIFEST_URL,
    RuntimeManifest, fetch_runtime_json,
};
use super::model::{JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeId};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::path::Path;
use tokio::fs as async_fs;
use tokio::task::JoinSet;

pub(super) async fn install_managed_runtime(
    component: &RuntimeId,
    dest_dir: &Path,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    let os_arch = runtime_os_arch();
    let parent_dir = dest_dir.parent().ok_or_else(|| {
        JavaRuntimeLookupError::Download("invalid runtime destination".to_string())
    })?;
    async_fs::create_dir_all(parent_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let temp_dir = dest_dir.with_extension("installing");
    remove_runtime_install_path_async(&temp_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    remove_runtime_install_path_async(dest_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::create_dir_all(&temp_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::write(temp_dir.join(".croopor-installing"), b"installing")
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let install_result = async {
        let all_runtimes = fetch_runtime_json::<RuntimeManifest>(RUNTIME_MANIFEST_URL).await?;

        let os_runtimes = all_runtimes.get(&os_arch).ok_or_else(|| {
            JavaRuntimeLookupError::Download(format!("no runtimes available for {os_arch}"))
        })?;
        let entries = os_runtimes.get(component.as_str()).ok_or_else(|| {
            JavaRuntimeLookupError::Download(format!(
                "runtime {} is not available for {os_arch}",
                component.as_str()
            ))
        })?;
        let manifest_url = entries
            .first()
            .map(|entry| entry.manifest.url.clone())
            .ok_or_else(|| {
                JavaRuntimeLookupError::Download(format!(
                    "runtime {} has no downloadable manifest for {os_arch}",
                    component.as_str()
                ))
            })?;

        let component_manifest = fetch_runtime_json::<ComponentManifest>(&manifest_url).await?;
        persist_component_manifest_proof(&temp_dir, &component_manifest).await?;

        install_runtime_manifest_files(
            component.as_str(),
            &temp_dir,
            component_manifest.files.clone(),
            observer,
        )
        .await?;

        let java_exe = java_executable(&temp_dir);
        if !runtime_executable_ready(&java_exe) {
            return Err(JavaRuntimeLookupError::Download(format!(
                "installed runtime {} is incomplete",
                component.as_str()
            )));
        }

        Ok(())
    }
    .await;

    if let Err(error) = install_result {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(error);
    }

    let _ = async_fs::remove_file(temp_dir.join(".croopor-installing")).await;
    if let Err(error) = async_fs::write(temp_dir.join(".croopor-ready"), b"ready").await {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    if let Err(error) = async_fs::rename(&temp_dir, dest_dir).await {
        let _ = async_fs::remove_dir_all(&temp_dir).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }

    Ok(())
}

async fn persist_component_manifest_proof(
    temp_dir: &Path,
    component_manifest: &ComponentManifest,
) -> Result<(), JavaRuntimeLookupError> {
    let bytes = serde_json::to_vec_pretty(component_manifest)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::write(temp_dir.join(COMPONENT_MANIFEST_PROOF_FILE), bytes)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

#[cfg(test)]
pub(super) fn remove_runtime_install_path(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        fs::remove_dir_all(path)
    } else {
        fs::remove_file(path)
    }
}

pub(super) async fn remove_runtime_install_path_async(path: &Path) -> std::io::Result<()> {
    let metadata = match async_fs::symlink_metadata(path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        async_fs::remove_dir_all(path).await
    } else {
        async_fs::remove_file(path).await
    }
}

pub(super) async fn install_runtime_manifest_files(
    component: &str,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    let plan = plan_runtime_manifest_files(files);
    let download_client = runtime_download_client();

    for (relative_path, file) in plan.directory_entries.into_iter().chain(plan.other_entries) {
        install_runtime_manifest_file(download_client.clone(), temp_dir, &relative_path, file)
            .await?;
    }

    let total_files = plan.file_entries.len();
    if total_files > 0 {
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.to_string(),
            current: 0,
            total: total_files,
            file: Some("Downloading runtime files".to_string()),
        });
    }

    let mut entries = plan.file_entries.into_iter();
    let mut tasks = JoinSet::new();
    for _ in 0..runtime_file_download_concurrency() {
        let Some(entry) = entries.next() else {
            break;
        };
        spawn_runtime_manifest_file_install(&mut tasks, download_client.clone(), temp_dir, entry);
    }

    let mut first_error = None;
    let mut completed_files = 0;
    while let Some(result) = tasks.join_next().await {
        match result {
            Ok(Ok(relative_path)) => {
                completed_files += 1;
                observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
                    component: component.to_string(),
                    current: completed_files,
                    total: total_files,
                    file: Some(bounded_manifest_file_label(&relative_path)),
                });
            }
            Ok(Err(error)) => {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
            Err(error) => {
                if first_error.is_none() {
                    first_error = Some(JavaRuntimeLookupError::Download(error.to_string()));
                }
            }
        }

        if first_error.is_none()
            && let Some(entry) = entries.next()
        {
            spawn_runtime_manifest_file_install(
                &mut tasks,
                download_client.clone(),
                temp_dir,
                entry,
            );
        }
    }

    if let Some(error) = first_error {
        return Err(error);
    }

    Ok(())
}

pub(super) fn spawn_runtime_manifest_file_install(
    tasks: &mut JoinSet<Result<String, JavaRuntimeLookupError>>,
    download_client: reqwest::Client,
    temp_dir: &Path,
    entry: (String, ComponentManifestFile),
) {
    let temp_dir = temp_dir.to_path_buf();
    let (relative_path, file) = entry;
    tasks.spawn(async move {
        let completed_path = relative_path.clone();
        Box::pin(install_runtime_manifest_file(
            download_client,
            &temp_dir,
            &relative_path,
            file,
        ))
        .await?;
        Ok(completed_path)
    });
}

#[derive(Debug, Default)]
pub(super) struct RuntimeManifestInstallPlan {
    pub(super) directory_entries: Vec<(String, ComponentManifestFile)>,
    pub(super) file_entries: Vec<(String, ComponentManifestFile)>,
    pub(super) other_entries: Vec<(String, ComponentManifestFile)>,
}

pub(super) fn plan_runtime_manifest_files(
    files: HashMap<String, ComponentManifestFile>,
) -> RuntimeManifestInstallPlan {
    let mut entries = files.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut plan = RuntimeManifestInstallPlan::default();
    for (relative_path, file) in entries {
        match file.kind.as_str() {
            "directory" => plan.directory_entries.push((relative_path, file)),
            "file" => plan.file_entries.push((relative_path, file)),
            _ => plan.other_entries.push((relative_path, file)),
        }
    }

    plan
}

pub(super) async fn install_runtime_manifest_file(
    download_client: reqwest::Client,
    temp_dir: &Path,
    relative_path: &str,
    file: ComponentManifestFile,
) -> Result<(), JavaRuntimeLookupError> {
    let destination = component_manifest_destination(temp_dir, relative_path)?;
    if file.kind == "directory" {
        async_fs::create_dir_all(&destination)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        return Ok(());
    }
    if file.kind != "file" {
        return Ok(());
    }
    let Some(raw) = file.downloads.and_then(|downloads| downloads.raw) else {
        return Err(JavaRuntimeLookupError::Download(format!(
            "runtime manifest file {} is missing download proof",
            bounded_manifest_file_label(relative_path)
        )));
    };
    if !raw.sha1.as_deref().is_some_and(runtime_sha1_is_valid) {
        return Err(JavaRuntimeLookupError::Download(format!(
            "runtime manifest file {} is missing checksum proof",
            bounded_manifest_file_label(relative_path)
        )));
    }

    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(parent)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    let temp_path = runtime_download_temp_path(&destination);
    let expected = RuntimeDownloadEvidence::from(&raw);
    Box::pin(fetch_runtime_file(
        &download_client,
        &raw.url,
        &temp_path,
        expected,
        relative_path,
    ))
    .await?;
    if let Err(error) = async_fs::rename(&temp_path, &destination).await {
        let _ = async_fs::remove_file(&temp_path).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    #[cfg(unix)]
    if file.executable {
        use std::os::unix::fs::PermissionsExt;

        let metadata = async_fs::metadata(&destination)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o755);
        async_fs::set_permissions(&destination, permissions)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    Ok(())
}

fn runtime_sha1_is_valid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
