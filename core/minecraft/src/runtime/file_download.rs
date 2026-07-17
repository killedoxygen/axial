use super::manifest::ComponentManifestDownload;
use super::model::{
    JavaRuntimeLookupError, RuntimeId, RuntimeSourceFailure, RuntimeSourceFailureKind,
};
use crate::artifact_path::ArtifactRelativePath;
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::borrow::Cow;
use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::sync::OnceLock;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

const MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 8;
const MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 32;
const RUNTIME_FILE_DOWNLOADS_PER_CORE: usize = 4;
const RUNTIME_DOWNLOAD_ATTEMPTS: u64 = 3;
const RUNTIME_DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS: u64 = 20;
const RUNTIME_DOWNLOAD_CLIENT_READ_TIMEOUT_SECS: u64 = 120;
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
    component: &RuntimeId,
    temp_dir: &Path,
    relative_path: &str,
) -> Result<PathBuf, JavaRuntimeLookupError> {
    component_manifest_destination_with_key(component, temp_dir, relative_path)
        .map(|(destination, _)| destination)
}

pub(super) fn component_manifest_destination_with_key(
    component: &RuntimeId,
    temp_dir: &Path,
    relative_path: &str,
) -> Result<(PathBuf, String), JavaRuntimeLookupError> {
    admitted_runtime_manifest_path(component, relative_path)
        .map(|(path, key)| (path.join_under(temp_dir), key))
}

fn admitted_runtime_manifest_path(
    component: &RuntimeId,
    relative_path: &str,
) -> Result<(ArtifactRelativePath, String), JavaRuntimeLookupError> {
    let path = ArtifactRelativePath::new(relative_path)
        .map_err(|_| unsafe_runtime_manifest_path(component, relative_path))?;
    let filesystem_key = path
        .portable_persisted_key()
        .map_err(|_| unsafe_runtime_manifest_path(component, relative_path))?;
    Ok((path, filesystem_key))
}

pub(super) fn component_manifest_link_target_path(
    component: &RuntimeId,
    component_root: &Path,
    link_destination: &Path,
    link_relative_path: &str,
    target: &str,
) -> Result<PathBuf, JavaRuntimeLookupError> {
    if target.trim().is_empty() || target.contains('\\') || Path::new(target).is_absolute() {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            format!(
                "unsafe runtime manifest link target for {}",
                bounded_manifest_file_label(link_relative_path)
            ),
        ));
    }
    for segment in target.split(['/', '\\']) {
        if matches!(segment, "" | "." | "..") {
            continue;
        }
        let portable_segment =
            ArtifactRelativePath::new(segment).and_then(|path| path.portable_persisted_key());
        if portable_segment.is_err() {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                format!(
                    "unsafe runtime manifest link target for {}",
                    bounded_manifest_file_label(link_relative_path)
                ),
            ));
        }
    }

    let root = normalize_path_lexically(component_root);
    let parent = link_destination.parent().unwrap_or(component_root);
    let target_path = normalize_path_lexically(&parent.join(target));
    if !target_path.starts_with(&root) {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::PolicyRejected,
            format!(
                "unsafe runtime manifest link target for {}",
                bounded_manifest_file_label(link_relative_path)
            ),
        ));
    }

    Ok(target_path)
}

fn unsafe_runtime_manifest_path(
    component: &RuntimeId,
    relative_path: &str,
) -> JavaRuntimeLookupError {
    runtime_source_failure(
        component,
        RuntimeSourceFailureKind::PolicyRejected,
        format!(
            "unsafe runtime manifest path: {}",
            bounded_manifest_file_label(relative_path)
        ),
    )
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            Component::Normal(value) => normalized.push(value),
            Component::RootDir => normalized.push(component.as_os_str()),
            Component::Prefix(prefix) => normalized.push(prefix.as_os_str()),
        }
    }
    normalized
}

pub(super) fn runtime_download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("runtime-download"))
        .to_os_string();
    name.push(".axial-tmp");
    destination.with_file_name(name)
}

pub(super) async fn fetch_runtime_file(
    component: &RuntimeId,
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let mut attempt = 1_u64;
    loop {
        let result = stream_runtime_file_to_temp_attempt(
            component,
            download_client,
            url,
            temp_path,
            &expected,
            relative_path,
        )
        .await;
        match result {
            Ok(()) => return Ok(()),
            Err(JavaRuntimeLookupError::RuntimeSource(failure))
                if failure.kind().is_retryable() && attempt < RUNTIME_DOWNLOAD_ATTEMPTS =>
            {
                let _ = async_fs::remove_file(runtime_filesystem_path(temp_path).as_ref()).await;
                tokio::time::sleep(std::time::Duration::from_millis(250 * attempt)).await;
                attempt += 1;
            }
            Err(error) => {
                let _ = async_fs::remove_file(runtime_filesystem_path(temp_path).as_ref()).await;
                return Err(error);
            }
        }
    }
}

async fn stream_runtime_file_to_temp_attempt(
    component: &RuntimeId,
    download_client: &reqwest::Client,
    url: &str,
    temp_path: &Path,
    expected: &RuntimeDownloadEvidence,
    relative_path: &str,
) -> Result<(), JavaRuntimeLookupError> {
    let response = download_client.get(url).send().await.map_err(|error| {
        let kind = if error.is_redirect() {
            RuntimeSourceFailureKind::PolicyRejected
        } else {
            RuntimeSourceFailureKind::Unavailable
        };
        runtime_source_failure(component, kind, error.to_string())
    })?;
    let status = response.status();
    if !status.is_success() {
        let kind = if status.is_server_error() || matches!(status.as_u16(), 408 | 425 | 429) {
            RuntimeSourceFailureKind::Unavailable
        } else {
            RuntimeSourceFailureKind::MetadataInvalid
        };
        return Err(runtime_source_failure(
            component,
            kind,
            format!("HTTP {status}"),
        ));
    }
    if let Some(expected_size) = expected.size
        && let Some(content_length) = response.content_length()
        && content_length > expected_size
    {
        return Err(runtime_source_failure(
            component,
            RuntimeSourceFailureKind::IntegrityMismatch,
            RuntimeDownloadIntegrityError::SizeMismatch {
                file: bounded_manifest_file_label(relative_path),
                expected: expected_size,
                actual: content_length,
            }
            .to_string(),
        ));
    }
    let mut output = async_fs::File::create(runtime_filesystem_path(temp_path).as_ref())
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut actual_size = 0_u64;

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::Unavailable,
                error.to_string(),
            )
        })?;
        let next_size = actual_size.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = expected.size
            && next_size > expected_size
        {
            return Err(runtime_source_failure(
                component,
                RuntimeSourceFailureKind::IntegrityMismatch,
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
            .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
        hasher.update(&chunk);
        actual_size = next_size;
    }
    output
        .flush()
        .await
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;

    let actual = RuntimeDownloadActual {
        size: actual_size,
        sha1: format!("{:x}", hasher.finalize()),
    };
    verify_runtime_download(relative_path, expected, &actual).map_err(|error| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::IntegrityMismatch,
            error.to_string(),
        )
    })
}

fn runtime_source_failure(
    component: &RuntimeId,
    kind: RuntimeSourceFailureKind,
    detail: impl Into<String>,
) -> JavaRuntimeLookupError {
    JavaRuntimeLookupError::RuntimeSource(RuntimeSourceFailure::new(
        component.clone(),
        kind,
        detail,
    ))
}

pub(super) fn runtime_download_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| {
            reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(
                    RUNTIME_DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS,
                ))
                .read_timeout(std::time::Duration::from_secs(
                    RUNTIME_DOWNLOAD_CLIENT_READ_TIMEOUT_SECS,
                ))
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if attempt.previous().len() >= 10 {
                        attempt.error("runtime file redirect limit exceeded")
                    } else if attempt.url().scheme() == "https" {
                        attempt.follow()
                    } else {
                        attempt.error("runtime file redirect must use HTTPS")
                    }
                }))
                .user_agent("axial/0.3")
                .pool_max_idle_per_host(MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY)
                .pool_idle_timeout(std::time::Duration::from_secs(
                    RUNTIME_DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS,
                ))
                .tcp_keepalive(std::time::Duration::from_secs(
                    RUNTIME_DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS,
                ))
                .build()
                .expect("runtime download HTTP client configuration should be valid")
        })
        .clone()
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

pub(super) fn runtime_filesystem_path(path: &Path) -> Cow<'_, Path> {
    #[cfg(windows)]
    {
        return windows_runtime_filesystem_path(path);
    }
    #[cfg(not(windows))]
    {
        Cow::Borrowed(path)
    }
}

#[cfg(windows)]
fn windows_runtime_filesystem_path(path: &Path) -> Cow<'_, Path> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
    };
    Cow::Owned(PathBuf::from(runtime_windows_verbatim_path_string(
        absolute.to_string_lossy().as_ref(),
    )))
}

#[cfg(any(windows, test))]
pub(super) fn runtime_windows_verbatim_path_string(path: &str) -> String {
    let normalized = path.replace('/', "\\");
    if normalized.starts_with(r"\\?\")
        || normalized.starts_with(r"\??\")
        || normalized.starts_with(r"\\.\")
    {
        return normalized;
    }
    if let Some(rest) = normalized.strip_prefix(r"\\") {
        return format!(r"\\?\UNC\{}", rest.trim_start_matches('\\'));
    }
    let bytes = normalized.as_bytes();
    if bytes.len() >= 3 && bytes[1] == b':' && bytes[0].is_ascii_alphabetic() && bytes[2] == b'\\' {
        return format!(r"\\?\{normalized}");
    }
    normalized
}
