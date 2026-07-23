use super::cancellation::RuntimeCancellationSet;
use super::manifest::ComponentManifestDownload;
use super::model::{
    JavaRuntimeLookupError, RuntimeId, RuntimeSourceFailure, RuntimeSourceFailureKind,
};
use crate::download::{
    ExpectedTransferDigests, ManagedTransferAuthority, RetryPolicy, SourceOnlyTransferTarget,
    TransferClient, TransferClientConfig, TransferContract, TransferFailureKind, TransferOrigin,
    TransferOutcome, VerifiedSource, start_source_transfer, transfer_cancellation_channel,
};
use crate::managed_fs::ManagedDir;
use crate::portable_path::{PortableFileName, PortablePathKey, PortableRelativePath};
use std::borrow::Cow;
use std::num::NonZeroU64;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

const MIN_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 8;
const MAX_RUNTIME_FILE_DOWNLOAD_CONCURRENCY: usize = 32;
const RUNTIME_FILE_DOWNLOADS_PER_CORE: usize = 4;
const RUNTIME_DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS: u64 = 20;
const RUNTIME_DOWNLOAD_CLIENT_READ_TIMEOUT_SECS: u64 = 120;
const RUNTIME_DOWNLOAD_CLIENT_REQUEST_TIMEOUT_SECS: u64 = 6 * 60 * 60;

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
) -> Result<(PathBuf, PortablePathKey), JavaRuntimeLookupError> {
    admitted_runtime_manifest_path(component, relative_path)
        .map(|(path, key)| (path.join_under(temp_dir), key))
}

fn admitted_runtime_manifest_path(
    component: &RuntimeId,
    relative_path: &str,
) -> Result<(PortableRelativePath, PortablePathKey), JavaRuntimeLookupError> {
    let path = PortableRelativePath::new_exact(relative_path)
        .map_err(|_| unsafe_runtime_manifest_path(component, relative_path))?;
    let filesystem_key = path.key();
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
        if PortableFileName::new_exact(segment).is_err() {
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

pub(super) struct RuntimeVerifiedSource {
    source: VerifiedSource,
    authority: ManagedTransferAuthority,
}

impl RuntimeVerifiedSource {
    pub(super) fn into_parts(self) -> (VerifiedSource, ManagedTransferAuthority) {
        (self.source, self.authority)
    }
}

pub(super) async fn fetch_runtime_source_until_cancelled(
    component: &RuntimeId,
    destination_root: &ManagedDir,
    client: TransferClient,
    url: &str,
    expected: RuntimeDownloadEvidence,
    relative_path: &str,
    cancellation: &mut RuntimeCancellationSet,
) -> Result<RuntimeVerifiedSource, JavaRuntimeLookupError> {
    let size = expected.size.and_then(NonZeroU64::new).ok_or_else(|| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            format!(
                "runtime file {} is missing exact size",
                bounded_manifest_file_label(relative_path)
            ),
        )
    })?;
    let digests =
        ExpectedTransferDigests::from_hex(expected.sha1.as_deref(), None).map_err(|error| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::MetadataInvalid,
                error.to_string(),
            )
        })?;
    let contract = TransferContract::authenticated_exact(size, digests).map_err(|error| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            error.to_string(),
        )
    })?;
    let parsed_url = reqwest::Url::parse(url).map_err(|error| {
        runtime_source_failure(
            component,
            RuntimeSourceFailureKind::MetadataInvalid,
            error.to_string(),
        )
    })?;
    let authority = ManagedTransferAuthority::retain(Arc::new(destination_root.clone()));
    let destination_name = runtime_transfer_destination_name();
    let destination = destination_root
        .admit_transient_destination(&destination_name)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let target = SourceOnlyTransferTarget::new(destination, authority.retained());
    let retry = RetryPolicy::classified(
        &[Duration::from_millis(250), Duration::from_millis(500)],
        runtime_transfer_retryable,
    )
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    let (transfer_sender, transfer_cancellation) = transfer_cancellation_channel();
    let task = start_source_transfer(
        client,
        parsed_url,
        target,
        contract,
        retry,
        transfer_cancellation,
    );
    let mut runtime_cancellation = cancellation.clone();
    let cancellation_bridge = tokio::spawn(async move {
        runtime_cancellation.cancelled().await;
        transfer_sender.cancel();
    });
    let outcome = task.join().await;
    cancellation_bridge.abort();
    let _ = cancellation_bridge.await;
    match outcome {
        TransferOutcome::Complete(source) => {
            if !source.shares_retained_authority(&authority) {
                return Err(JavaRuntimeLookupError::Install(
                    "runtime transfer returned unrelated authority".to_string(),
                ));
            }
            Ok(RuntimeVerifiedSource { source, authority })
        }
        TransferOutcome::Failed {
            report,
            authority: terminal,
        } => {
            destination_root
                .settle()
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
            if !terminal.shares_retained_authority(&authority) {
                return Err(JavaRuntimeLookupError::Install(
                    "runtime transfer failure returned unrelated authority".to_string(),
                ));
            }
            Err(runtime_transfer_failure(
                component,
                relative_path,
                report.last(),
            ))
        }
        TransferOutcome::CleanupPending(obligation) => {
            let failure =
                runtime_transfer_failure(component, relative_path, obligation.report().last());
            destination_root
                .retain_transfer_cleanup(obligation, authority)
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
            Err(failure)
        }
        TransferOutcome::Unsettled(obligation) => {
            if !obligation.shares_retained_authority(&authority) {
                return Err(JavaRuntimeLookupError::Install(
                    "unsettled runtime transfer returned unrelated authority".to_string(),
                ));
            }
            let failure =
                runtime_transfer_failure(component, relative_path, obligation.report().last());
            let settlement = destination_root
                .settle_transfer_effects(&authority)
                .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
            let (_report, terminal) = obligation
                .reconcile_after_effect_settlement(&settlement)
                .map_err(|_| {
                    JavaRuntimeLookupError::Install(
                        "runtime transfer effect settlement was refused".to_string(),
                    )
                })?;
            if !terminal.shares_retained_authority(&authority) {
                return Err(JavaRuntimeLookupError::Install(
                    "settled runtime transfer returned unrelated authority".to_string(),
                ));
            }
            Err(failure)
        }
    }
}

fn runtime_download_cancelled() -> JavaRuntimeLookupError {
    JavaRuntimeLookupError::Install("runtime staging was cancelled".to_string())
}

fn runtime_transfer_destination_name() -> String {
    format!(".axial-runtime-source-{}", uuid::Uuid::new_v4().simple())
}

fn runtime_transfer_retryable(failure: &TransferFailureKind) -> bool {
    matches!(
        failure,
        TransferFailureKind::Network
            | TransferFailureKind::ProviderStatus(408 | 425 | 429 | 500..=599)
    )
}

fn runtime_transfer_origin(
    url: &reqwest::Url,
) -> Result<TransferOrigin, crate::download::TransferOriginError> {
    #[cfg(any(test, feature = "test-support"))]
    if url.scheme() == "http" {
        return TransferOrigin::from_loopback_http_for_test_support(url);
    }
    TransferOrigin::from_url(url)
}

pub(super) fn runtime_transfer_client<'a>(
    component: &RuntimeId,
    urls: impl IntoIterator<Item = &'a str>,
) -> Result<TransferClient, JavaRuntimeLookupError> {
    let mut origins = Vec::new();
    for url in urls {
        let parsed = reqwest::Url::parse(url).map_err(|error| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::MetadataInvalid,
                error.to_string(),
            )
        })?;
        let origin = runtime_transfer_origin(&parsed).map_err(|error| {
            runtime_source_failure(
                component,
                RuntimeSourceFailureKind::PolicyRejected,
                error.to_string(),
            )
        })?;
        if !origins.contains(&origin) {
            origins.push(origin);
        }
    }
    let config = TransferClientConfig::bounded(
        Duration::from_secs(RUNTIME_DOWNLOAD_CLIENT_CONNECT_TIMEOUT_SECS),
        Duration::from_secs(RUNTIME_DOWNLOAD_CLIENT_READ_TIMEOUT_SECS),
        Duration::from_secs(RUNTIME_DOWNLOAD_CLIENT_REQUEST_TIMEOUT_SECS),
        origins,
    )
    .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))?;
    TransferClient::build(config)
        .map_err(|error| JavaRuntimeLookupError::Install(error.to_string()))
}

fn runtime_transfer_failure(
    component: &RuntimeId,
    relative_path: &str,
    failure: TransferFailureKind,
) -> JavaRuntimeLookupError {
    if failure == TransferFailureKind::Cancelled {
        return runtime_download_cancelled();
    }
    let kind = match failure {
        TransferFailureKind::Network
        | TransferFailureKind::ProviderStatus(408 | 425 | 429 | 500..=599) => {
            RuntimeSourceFailureKind::Unavailable
        }
        TransferFailureKind::RequestPolicy | TransferFailureKind::ContentEncodingRejected => {
            RuntimeSourceFailureKind::PolicyRejected
        }
        TransferFailureKind::ContentLengthContractMismatch { .. }
        | TransferFailureKind::ContentLengthMismatch { .. }
        | TransferFailureKind::ByteLimitExceeded { .. }
        | TransferFailureKind::SizeMismatch { .. }
        | TransferFailureKind::ByteCountOverflow
        | TransferFailureKind::ProducerWorkerMismatch { .. }
        | TransferFailureKind::DigestMismatch(_) => RuntimeSourceFailureKind::IntegrityMismatch,
        TransferFailureKind::ProviderStatus(_) => RuntimeSourceFailureKind::MetadataInvalid,
        TransferFailureKind::StageCreate(_)
        | TransferFailureKind::StageWrite(_)
        | TransferFailureKind::StageSeal(_)
        | TransferFailureKind::ChannelClosed
        | TransferFailureKind::WorkerStopped => {
            return JavaRuntimeLookupError::Install(format!(
                "runtime file {} transfer failed: {failure:?}",
                bounded_manifest_file_label(relative_path)
            ));
        }
        TransferFailureKind::Cancelled => unreachable!("cancelled transfer handled above"),
    };
    runtime_source_failure(
        component,
        kind,
        format!(
            "runtime file {} transfer failed: {failure:?}",
            bounded_manifest_file_label(relative_path)
        ),
    )
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
