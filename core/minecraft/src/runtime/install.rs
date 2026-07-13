use super::file_download::{
    RuntimeDownloadActual, RuntimeDownloadEvidence, bounded_manifest_file_label,
    component_manifest_destination, component_manifest_link_target_path, fetch_runtime_file,
    runtime_download_client, runtime_download_temp_path, runtime_file_download_concurrency,
    runtime_filesystem_path, verify_runtime_download,
};
use super::layout::{java_executable, runtime_executable_ready};
use super::manifest::{
    COMPONENT_MANIFEST_PROOF_FILE, ComponentManifest, ComponentManifestDownload,
    ComponentManifestDownloads, ComponentManifestFile, RuntimeSourceReceipt,
    component_manifest_proof_bytes,
};
use super::model::{JavaRuntimeLookupError, RuntimeEnsureEvent, RuntimeId};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::collections::HashMap;
#[cfg(test)]
use std::fs;
use std::io::{BufReader, Write};
use std::path::Path;
use tokio::fs as async_fs;

const MAX_EPHEMERAL_RUNTIME_ENTRIES: usize = 4096;
const MAX_EPHEMERAL_RUNTIME_DEPTH: usize = 16;
const MAX_EPHEMERAL_RUNTIME_FILE_BYTES: u64 = 128 << 20;
const MAX_EPHEMERAL_RUNTIME_TOTAL_BYTES: u64 = 512 << 20;

pub(super) async fn install_managed_runtime(
    component: &RuntimeId,
    dest_dir: &Path,
    source: &RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    install_managed_runtime_with_concurrency(
        component,
        dest_dir,
        source,
        observer,
        runtime_file_download_concurrency(),
    )
    .await
}

pub(super) async fn install_ephemeral_processor_runtime(
    component: &RuntimeId,
    dest_dir: &Path,
    source: &RuntimeSourceReceipt,
    max_entries: usize,
    max_bytes: u64,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    validate_ephemeral_processor_manifest(source, max_entries, max_bytes)?;
    install_managed_runtime_with_concurrency(component, dest_dir, source, observer, 1).await
}

async fn install_managed_runtime_with_concurrency(
    component: &RuntimeId,
    dest_dir: &Path,
    source: &RuntimeSourceReceipt,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
) -> Result<(), JavaRuntimeLookupError> {
    if source.component() != component {
        return Err(JavaRuntimeLookupError::Download(
            "runtime source component mismatch".to_string(),
        ));
    }
    let parent_dir = dest_dir.parent().ok_or_else(|| {
        JavaRuntimeLookupError::Download("invalid runtime destination".to_string())
    })?;
    async_fs::create_dir_all(runtime_filesystem_path(parent_dir).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let temp_dir = dest_dir.with_extension("installing");
    remove_runtime_install_path_async(&temp_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    remove_runtime_install_path_async(dest_dir)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    async_fs::create_dir_all(runtime_filesystem_path(&temp_dir).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let installing_marker = temp_dir.join(".axial-installing");
    async_fs::write(
        runtime_filesystem_path(&installing_marker).as_ref(),
        b"installing",
    )
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let install_result = async {
        let component_manifest = source.manifest();
        persist_component_manifest_proof(&temp_dir, component_manifest).await?;

        install_runtime_manifest_files_with_concurrency(
            component.as_str(),
            &temp_dir,
            component_manifest.files.clone(),
            observer,
            download_concurrency,
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
        let _ = async_fs::remove_dir_all(runtime_filesystem_path(&temp_dir).as_ref()).await;
        return Err(error);
    }

    let _ = async_fs::remove_file(runtime_filesystem_path(&installing_marker).as_ref()).await;
    let ready_marker = temp_dir.join(".axial-ready");
    if let Err(error) =
        async_fs::write(runtime_filesystem_path(&ready_marker).as_ref(), b"ready").await
    {
        let _ = async_fs::remove_dir_all(runtime_filesystem_path(&temp_dir).as_ref()).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    if let Err(error) = async_fs::rename(
        runtime_filesystem_path(&temp_dir).as_ref(),
        runtime_filesystem_path(dest_dir).as_ref(),
    )
    .await
    {
        let _ = async_fs::remove_dir_all(runtime_filesystem_path(&temp_dir).as_ref()).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    observer(RuntimeEnsureEvent::ManagedRuntimeReady {
        component: component.as_str().to_string(),
    });

    Ok(())
}

fn validate_ephemeral_processor_manifest(
    source: &RuntimeSourceReceipt,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    validate_ephemeral_processor_manifest_contract(
        source.manifest(),
        source.bytes().len() as u64,
        max_entries.min(MAX_EPHEMERAL_RUNTIME_ENTRIES),
        max_bytes.min(MAX_EPHEMERAL_RUNTIME_TOTAL_BYTES),
    )
}

fn validate_ephemeral_processor_manifest_contract(
    manifest: &ComponentManifest,
    manifest_bytes: u64,
    max_entries: usize,
    max_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    if manifest.files.len() > max_entries {
        return Err(JavaRuntimeLookupError::Download(
            "processor runtime manifest exceeds the entry bound".to_string(),
        ));
    }
    let mut filesystem_entries = std::collections::HashSet::new();
    let mut raw_total = 0_u64;
    let mut largest_compressed = 0_u64;
    let mut has_compressed = false;
    for (relative_path, file) in &manifest.files {
        component_manifest_destination(Path::new("runtime"), relative_path)?;
        let segments = relative_path.split(['/', '\\']).collect::<Vec<_>>();
        if segments.len() > MAX_EPHEMERAL_RUNTIME_DEPTH {
            return Err(JavaRuntimeLookupError::Download(
                "processor runtime manifest exceeds the depth bound".to_string(),
            ));
        }
        let mut prefix = String::new();
        for segment in segments {
            if !prefix.is_empty() {
                prefix.push('/');
            }
            prefix.push_str(segment);
            filesystem_entries.insert(prefix.clone());
        }
        match file.kind.as_str() {
            "directory" | "link" => {}
            "file" => {
                let downloads = file.downloads.as_ref().ok_or_else(|| {
                    JavaRuntimeLookupError::Download(
                        "processor runtime file is missing exact download proof".to_string(),
                    )
                })?;
                let raw = downloads.raw.as_ref().ok_or_else(|| {
                    JavaRuntimeLookupError::Download(
                        "processor runtime file is missing exact raw proof".to_string(),
                    )
                })?;
                let raw_size = exact_ephemeral_runtime_download_size(raw, "raw")?;
                raw_total = raw_total.checked_add(raw_size).ok_or_else(|| {
                    JavaRuntimeLookupError::Download(
                        "processor runtime byte total overflowed".to_string(),
                    )
                })?;
                if let Some(lzma) = downloads.lzma.as_ref() {
                    has_compressed = true;
                    largest_compressed = largest_compressed
                        .max(exact_ephemeral_runtime_download_size(lzma, "compressed")?);
                }
            }
            _ => {
                return Err(JavaRuntimeLookupError::Download(
                    "processor runtime manifest contains an unsupported entry".to_string(),
                ));
            }
        }
    }
    let admitted_total = raw_total
        .checked_add(largest_compressed)
        .and_then(|total| total.checked_add(manifest_bytes))
        .and_then(|total| total.checked_add(64))
        .ok_or_else(|| {
            JavaRuntimeLookupError::Download("processor runtime byte total overflowed".to_string())
        })?;
    let transient_entries = 3_usize.saturating_add(usize::from(has_compressed));
    if filesystem_entries.len().saturating_add(transient_entries) > max_entries
        || raw_total > max_bytes
        || admitted_total > max_bytes
    {
        return Err(JavaRuntimeLookupError::Download(
            "processor runtime manifest exceeds the aggregate bound".to_string(),
        ));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn validate_ephemeral_processor_manifest_for_test(
    manifest: &ComponentManifest,
    manifest_bytes: u64,
) -> Result<(), JavaRuntimeLookupError> {
    validate_ephemeral_processor_manifest_contract(
        manifest,
        manifest_bytes,
        MAX_EPHEMERAL_RUNTIME_ENTRIES,
        MAX_EPHEMERAL_RUNTIME_TOTAL_BYTES,
    )
}

fn exact_ephemeral_runtime_download_size(
    download: &ComponentManifestDownload,
    label: &str,
) -> Result<u64, JavaRuntimeLookupError> {
    let size = download.size.filter(|size| *size > 0).ok_or_else(|| {
        JavaRuntimeLookupError::Download(format!(
            "processor runtime {label} file is missing exact size"
        ))
    })?;
    if size > MAX_EPHEMERAL_RUNTIME_FILE_BYTES {
        return Err(JavaRuntimeLookupError::Download(format!(
            "processor runtime {label} file exceeds the per-file bound"
        )));
    }
    if !download.sha1.as_deref().is_some_and(runtime_sha1_is_valid) {
        return Err(JavaRuntimeLookupError::Download(format!(
            "processor runtime {label} file is missing exact checksum"
        )));
    }
    Ok(size)
}

async fn persist_component_manifest_proof(
    temp_dir: &Path,
    component_manifest: &ComponentManifest,
) -> Result<(), JavaRuntimeLookupError> {
    let bytes = component_manifest_proof_bytes(component_manifest)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let proof_path = temp_dir.join(COMPONENT_MANIFEST_PROOF_FILE);
    async_fs::write(runtime_filesystem_path(&proof_path).as_ref(), bytes)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

#[cfg(test)]
pub(super) fn remove_runtime_install_path(path: &Path) -> std::io::Result<()> {
    let metadata = match fs::symlink_metadata(runtime_filesystem_path(path).as_ref()) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        fs::remove_dir_all(runtime_filesystem_path(path).as_ref())
    } else {
        fs::remove_file(runtime_filesystem_path(path).as_ref())
    }
}

pub(super) async fn remove_runtime_install_path_async(path: &Path) -> std::io::Result<()> {
    let metadata = match async_fs::symlink_metadata(runtime_filesystem_path(path).as_ref()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };

    if metadata.is_dir() {
        async_fs::remove_dir_all(runtime_filesystem_path(path).as_ref()).await
    } else {
        async_fs::remove_file(runtime_filesystem_path(path).as_ref()).await
    }
}

#[cfg(test)]
pub(super) async fn install_runtime_manifest_files(
    component: &str,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
) -> Result<(), JavaRuntimeLookupError> {
    install_runtime_manifest_files_with_concurrency(
        component,
        temp_dir,
        files,
        observer,
        runtime_file_download_concurrency(),
    )
    .await
}

async fn install_runtime_manifest_files_with_concurrency(
    component: &str,
    temp_dir: &Path,
    files: HashMap<String, ComponentManifestFile>,
    observer: &mut impl FnMut(RuntimeEnsureEvent),
    download_concurrency: usize,
) -> Result<(), JavaRuntimeLookupError> {
    let plan = plan_runtime_manifest_files(files);
    let download_client = runtime_download_client();

    for (relative_path, file) in plan.directory_entries.into_iter().chain(plan.other_entries) {
        install_runtime_manifest_file(download_client.clone(), temp_dir, &relative_path, file)
            .await?;
    }

    let total_files = plan.file_entries.len() + plan.link_entries.len();
    let total_bytes = plan
        .file_entries
        .iter()
        .map(|(_, file)| runtime_manifest_file_bytes(file))
        .sum::<u64>();
    if total_files > 0 {
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.to_string(),
            current: 0,
            total: total_files,
            bytes_done: 0,
            bytes_total: total_bytes,
        });
    }

    let mut file_downloads =
        futures_util::stream::iter(plan.file_entries.into_iter().map(|entry| {
            let download_client = download_client.clone();
            let temp_dir = temp_dir.to_path_buf();
            async move {
                let (relative_path, file) = entry;
                let bytes = runtime_manifest_file_bytes(&file);
                Box::pin(install_runtime_manifest_file(
                    download_client,
                    &temp_dir,
                    &relative_path,
                    file,
                ))
                .await?;
                Ok::<CompletedRuntimeManifestFile, JavaRuntimeLookupError>(
                    CompletedRuntimeManifestFile { bytes },
                )
            }
        }))
        .buffer_unordered(download_concurrency.max(1));

    let mut completed_files = 0;
    let mut completed_bytes = 0_u64;
    while let Some(result) = file_downloads.next().await {
        let completed = result?;
        completed_files += 1;
        completed_bytes += completed.bytes;
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.to_string(),
            current: completed_files,
            total: total_files,
            bytes_done: completed_bytes,
            bytes_total: total_bytes,
        });
    }

    for (relative_path, file) in plan.link_entries {
        install_runtime_manifest_file(download_client.clone(), temp_dir, &relative_path, file)
            .await?;
        completed_files += 1;
        observer(RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component: component.to_string(),
            current: completed_files,
            total: total_files,
            bytes_done: completed_bytes,
            bytes_total: total_bytes,
        });
    }

    Ok(())
}

pub(super) struct CompletedRuntimeManifestFile {
    pub(super) bytes: u64,
}

pub(super) fn runtime_manifest_file_bytes(file: &ComponentManifestFile) -> u64 {
    file.downloads
        .as_ref()
        .and_then(|downloads| downloads.lzma.as_ref().or(downloads.raw.as_ref()))
        .and_then(|raw| raw.size)
        .unwrap_or(0)
}

#[derive(Debug, Default)]
pub(crate) struct RuntimeManifestInstallPlan {
    pub(crate) directory_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) file_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) link_entries: Vec<(String, ComponentManifestFile)>,
    pub(crate) other_entries: Vec<(String, ComponentManifestFile)>,
}

pub(crate) fn plan_runtime_manifest_files(
    files: HashMap<String, ComponentManifestFile>,
) -> RuntimeManifestInstallPlan {
    let mut entries = files.into_iter().collect::<Vec<_>>();
    entries.sort_by(|(left, _), (right, _)| left.cmp(right));

    let mut plan = RuntimeManifestInstallPlan::default();
    for (relative_path, file) in entries {
        match file.kind.as_str() {
            "directory" => plan.directory_entries.push((relative_path, file)),
            "file" => plan.file_entries.push((relative_path, file)),
            "link" => plan.link_entries.push((relative_path, file)),
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
        async_fs::create_dir_all(runtime_filesystem_path(&destination).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        return Ok(());
    }
    if file.kind == "link" {
        return install_runtime_manifest_link(temp_dir, &destination, relative_path, &file).await;
    }
    if file.kind != "file" {
        return Err(JavaRuntimeLookupError::Download(format!(
            "unsupported runtime manifest entry {} ({})",
            bounded_manifest_file_label(relative_path),
            file.kind
        )));
    }
    let RuntimeFileDownloadSelection { raw, lzma } =
        select_runtime_file_downloads(relative_path, file.downloads)?;

    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(runtime_filesystem_path(parent).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    let temp_path = runtime_download_temp_path(&destination);
    if let Some(lzma) = lzma {
        Box::pin(fetch_lzma_runtime_file(
            &download_client,
            &lzma,
            &raw,
            &temp_path,
            relative_path,
        ))
        .await?;
    } else {
        let expected = RuntimeDownloadEvidence::from(&raw);
        Box::pin(fetch_runtime_file(
            &download_client,
            &raw.url,
            &temp_path,
            expected,
            relative_path,
        ))
        .await?;
    }
    if let Err(error) = async_fs::rename(
        runtime_filesystem_path(&temp_path).as_ref(),
        runtime_filesystem_path(&destination).as_ref(),
    )
    .await
    {
        let _ = async_fs::remove_file(runtime_filesystem_path(&temp_path).as_ref()).await;
        return Err(JavaRuntimeLookupError::Download(error.to_string()));
    }
    #[cfg(unix)]
    if file.executable {
        use std::os::unix::fs::PermissionsExt;

        let permissions = std::fs::Permissions::from_mode(0o755);
        async_fs::set_permissions(runtime_filesystem_path(&destination).as_ref(), permissions)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    Ok(())
}

struct RuntimeFileDownloadSelection {
    raw: ComponentManifestDownload,
    lzma: Option<ComponentManifestDownload>,
}

fn select_runtime_file_downloads(
    relative_path: &str,
    downloads: Option<ComponentManifestDownloads>,
) -> Result<RuntimeFileDownloadSelection, JavaRuntimeLookupError> {
    let Some(downloads) = downloads else {
        return Err(JavaRuntimeLookupError::Download(format!(
            "runtime manifest file {} is missing download proof",
            bounded_manifest_file_label(relative_path)
        )));
    };
    let Some(raw) = downloads.raw else {
        return Err(JavaRuntimeLookupError::Download(format!(
            "runtime manifest file {} is missing download proof",
            bounded_manifest_file_label(relative_path)
        )));
    };
    validate_runtime_download_checksum(relative_path, &raw, "file")?;
    if let Some(lzma) = downloads.lzma.as_ref() {
        validate_runtime_download_checksum(relative_path, lzma, "lzma file")?;
    }
    Ok(RuntimeFileDownloadSelection {
        raw,
        lzma: downloads.lzma,
    })
}

fn validate_runtime_download_checksum(
    relative_path: &str,
    download: &ComponentManifestDownload,
    label: &str,
) -> Result<(), JavaRuntimeLookupError> {
    if download.sha1.as_deref().is_some_and(runtime_sha1_is_valid) {
        return Ok(());
    }
    Err(JavaRuntimeLookupError::Download(format!(
        "runtime manifest {label} {} is missing checksum proof",
        bounded_manifest_file_label(relative_path)
    )))
}

async fn fetch_lzma_runtime_file(
    download_client: &reqwest::Client,
    lzma: &ComponentManifestDownload,
    raw: &ComponentManifestDownload,
    temp_path: &Path,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let lzma_temp_path = runtime_lzma_download_temp_path(temp_path);
    let compressed_expected = RuntimeDownloadEvidence::from(lzma);
    let raw_expected = RuntimeDownloadEvidence::from(raw);
    let result = async {
        Box::pin(fetch_runtime_file(
            download_client,
            &lzma.url,
            &lzma_temp_path,
            compressed_expected,
            relative_path,
        ))
        .await?;
        decompress_lzma_runtime_file_to_temp(
            &lzma_temp_path,
            temp_path,
            raw_expected,
            relative_path.to_string(),
        )
        .await
    }
    .await;

    let _ = async_fs::remove_file(runtime_filesystem_path(&lzma_temp_path).as_ref()).await;
    if result.is_err() {
        let _ = async_fs::remove_file(runtime_filesystem_path(temp_path).as_ref()).await;
    }
    result
}

fn runtime_lzma_download_temp_path(temp_path: &Path) -> std::path::PathBuf {
    let mut name = temp_path
        .file_name()
        .unwrap_or_else(|| std::ffi::OsStr::new("runtime-download"))
        .to_os_string();
    name.push(".lzma");
    temp_path.with_file_name(name)
}

async fn decompress_lzma_runtime_file_to_temp(
    compressed_path: &Path,
    output_path: &Path,
    expected: RuntimeDownloadEvidence,
    relative_path: String,
) -> Result<(), JavaRuntimeLookupError> {
    let compressed_path = compressed_path.to_path_buf();
    let output_path = output_path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let input = std::fs::File::open(runtime_filesystem_path(&compressed_path).as_ref())
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let output = std::fs::File::create(runtime_filesystem_path(&output_path).as_ref())
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let mut input = BufReader::new(input);
        let mut output = RuntimeIntegrityWriter::new(output, expected.clone(), &relative_path);
        lzma_rs::lzma_decompress(&mut input, &mut output)
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        output
            .flush()
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let actual = output.actual();
        verify_runtime_download(&relative_path, &expected, &actual)
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?
}

struct RuntimeIntegrityWriter {
    output: std::fs::File,
    expected: RuntimeDownloadEvidence,
    relative_path: String,
    hasher: Sha1,
    size: u64,
}

impl RuntimeIntegrityWriter {
    fn new(output: std::fs::File, expected: RuntimeDownloadEvidence, relative_path: &str) -> Self {
        Self {
            output,
            expected,
            relative_path: relative_path.to_string(),
            hasher: Sha1::new(),
            size: 0,
        }
    }

    fn actual(self) -> RuntimeDownloadActual {
        RuntimeDownloadActual {
            size: self.size,
            sha1: format!("{:x}", self.hasher.finalize()),
        }
    }
}

impl Write for RuntimeIntegrityWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        let next_size = self.size.saturating_add(buffer.len() as u64);
        if let Some(expected_size) = self.expected.size
            && next_size > expected_size
        {
            return Err(std::io::Error::other(
                super::file_download::RuntimeDownloadIntegrityError::SizeMismatch {
                    file: bounded_manifest_file_label(&self.relative_path),
                    expected: expected_size,
                    actual: next_size,
                }
                .to_string(),
            ));
        }
        let written = self.output.write(buffer)?;
        self.hasher.update(&buffer[..written]);
        self.size += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.output.flush()
    }
}

async fn install_runtime_manifest_link(
    temp_dir: &Path,
    destination: &Path,
    relative_path: &str,
    file: &ComponentManifestFile,
) -> Result<(), JavaRuntimeLookupError> {
    let Some(target) = file.target.as_deref() else {
        return Err(JavaRuntimeLookupError::Download(format!(
            "runtime manifest link {} is missing target",
            bounded_manifest_file_label(relative_path)
        )));
    };
    component_manifest_link_target_path(temp_dir, destination, relative_path, target)?;
    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(runtime_filesystem_path(parent).as_ref())
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    }

    install_runtime_manifest_symlink(target.to_string(), destination.to_path_buf()).await
}

#[cfg(unix)]
async fn install_runtime_manifest_symlink(
    target: String,
    destination: std::path::PathBuf,
) -> Result<(), JavaRuntimeLookupError> {
    tokio::task::spawn_blocking(move || {
        std::os::unix::fs::symlink(target, runtime_filesystem_path(&destination).as_ref())
    })
    .await
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?
    .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

#[cfg(not(unix))]
async fn install_runtime_manifest_symlink(
    _target: String,
    _destination: std::path::PathBuf,
) -> Result<(), JavaRuntimeLookupError> {
    Err(JavaRuntimeLookupError::Download(
        "runtime manifest link entries are unsupported on this platform".to_string(),
    ))
}

fn runtime_sha1_is_valid(value: &str) -> bool {
    value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}
