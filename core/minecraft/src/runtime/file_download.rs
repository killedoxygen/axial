use super::manifest::ComponentManifestDownload;
use super::model::JavaRuntimeLookupError;
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

const MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 2;
const MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 8;
const RUNTIME_FILE_DOWNLOADS_PER_CORE: usize = 2;
const RUNTIME_DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 120;
const RUNTIME_DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;

pub(super) fn runtime_file_download_concurrency() -> usize {
    runtime_file_download_concurrency_for(available_runtime_parallelism())
}

pub(super) fn available_runtime_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY)
}

pub(super) fn runtime_file_download_concurrency_for(cores: usize) -> usize {
    cores.saturating_mul(RUNTIME_FILE_DOWNLOADS_PER_CORE).clamp(
        MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY,
        MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY,
    )
}

pub(super) fn component_manifest_destination(
    temp_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, JavaRuntimeLookupError> {
    if relative_path.is_empty() || has_unsafe_path_component(Path::new(relative_path)) {
        return Err(JavaRuntimeLookupError::Download(format!(
            "unsafe runtime manifest path: {}",
            bounded_manifest_file_label(relative_path)
        )));
    }

    let mut destination = temp_dir.to_path_buf();
    for segment in relative_path.split(['/', '\\']) {
        if segment.is_empty()
            || segment.contains(':')
            || has_unsafe_path_component(Path::new(segment))
        {
            return Err(JavaRuntimeLookupError::Download(format!(
                "unsafe runtime manifest path: {}",
                bounded_manifest_file_label(relative_path)
            )));
        }
        destination.push(segment);
    }

    Ok(destination)
}

pub(super) fn has_unsafe_path_component(path: &Path) -> bool {
    path.components()
        .any(|component| !matches!(component, std::path::Component::Normal(_)))
}

pub(super) fn runtime_download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("runtime-download"))
        .to_os_string();
    name.push(".croopor-tmp");
    destination.with_file_name(name)
}

pub(super) async fn fetch_runtime_file(
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let result =
        stream_runtime_file_to_temp(download_client, url, temp_path, &expected, relative_path)
            .await;

    if result.is_err() {
        let _ = async_fs::remove_file(temp_path).await;
    }

    result
}

pub(super) async fn stream_runtime_file_to_temp(
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: &RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let response = download_client
        .get(url)
        .send()
        .await
        .and_then(reqwest::Response::error_for_status)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    if let Some(expected_size) = expected.size
        && let Some(content_length) = response.content_length()
        && content_length > expected_size
    {
        return Err(JavaRuntimeLookupError::Download(
            RuntimeDownloadIntegrityError::SizeMismatch {
                file: bounded_manifest_file_label(relative_path),
                expected: expected_size,
                actual: content_length,
            }
            .to_string(),
        ));
    }
    let mut output = async_fs::File::create(temp_path)
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut actual_size = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        let next_size = actual_size.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = expected.size
            && next_size > expected_size
        {
            return Err(JavaRuntimeLookupError::Download(
                RuntimeDownloadIntegrityError::SizeMismatch {
                    file: bounded_manifest_file_label(relative_path),
                    expected: expected_size,
                    actual: next_size,
                }
                .to_string(),
            ));
        }
        output
            .write_all(&chunk)
            .await
            .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
        hasher.update(&chunk);
        actual_size = next_size;
    }
    output
        .flush()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;
    output
        .sync_all()
        .await
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))?;

    let actual = RuntimeDownloadActual {
        size: actual_size,
        sha1: format!("{:x}", hasher.finalize()),
    };
    verify_runtime_download(relative_path, expected, &actual)
        .map_err(|error| JavaRuntimeLookupError::Download(error.to_string()))
}

pub(super) fn runtime_download_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("croopor/0.3")
        .pool_max_idle_per_host(MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY)
        .pool_idle_timeout(std::time::Duration::from_secs(
            RUNTIME_DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS,
        ))
        .tcp_keepalive(std::time::Duration::from_secs(
            RUNTIME_DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS,
        ))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimeDownloadEvidence {
    pub(super) size: Option<u64>,
    pub(super) sha1: Option<String>,
}

impl From<&ComponentManifestDownload> for RuntimeDownloadEvidence {
    fn from(download: &ComponentManifestDownload) -> Self {
        Self {
            size: download.size,
            sha1: download.sha1.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimeDownloadActual {
    pub(super) size: u64,
    pub(super) sha1: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum RuntimeDownloadIntegrityError {
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Sha1Mismatch {
        file: String,
        expected: String,
        actual: String,
    },
}

impl std::fmt::Display for RuntimeDownloadIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "runtime file {file} size mismatch: expected {expected}, got {actual}"
            ),
            Self::Sha1Mismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "runtime file {file} sha1 mismatch: expected {expected}, got {actual}"
            ),
        }
    }
}

pub(super) fn verify_runtime_download(
    relative_path: &str,
    expected: &RuntimeDownloadEvidence,
    actual: &RuntimeDownloadActual,
) -> Result<(), RuntimeDownloadIntegrityError> {
    let file = bounded_manifest_file_label(relative_path);
    if let Some(expected_size) = expected.size
        && actual.size != expected_size
    {
        return Err(RuntimeDownloadIntegrityError::SizeMismatch {
            file,
            expected: expected_size,
            actual: actual.size,
        });
    }

    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let expected_sha1 = expected_sha1.trim();
        if !actual.sha1.eq_ignore_ascii_case(expected_sha1) {
            return Err(RuntimeDownloadIntegrityError::Sha1Mismatch {
                file,
                expected: expected_sha1.to_string(),
                actual: actual.sha1.clone(),
            });
        }
    }

    Ok(())
}

pub(super) fn bounded_manifest_file_label(relative_path: &str) -> String {
    const MAX_LABEL_CHARS: usize = 120;
    let sanitized = relative_path.replace(['\r', '\n'], "?");
    let mut chars = sanitized.chars();
    let label = chars.by_ref().take(MAX_LABEL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{label}...")
    } else {
        label
    }
}
