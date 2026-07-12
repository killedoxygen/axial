use super::facts::{
    ExecutionDownloadRequest, emit_execution_download_facts, emit_selected_download_descriptor,
    execution_download_error, execution_download_fact, integrity_mismatch_fact, metadata_facts,
    no_download_fact_fields, selected_artifact_missing_fact, selected_download_target_label,
    size_mismatch_fact,
};
use super::integrity::{
    ExistingArtifactIntegrity, checksumless_jar_is_readable_async, download_size_mismatch,
    existing_artifact_integrity, existing_content_addressed_asset_integrity, is_sha1_hex,
    verify_download_integrity,
};
use super::model::{
    ActualIntegrity, DownloadError, DownloadIntegrityError, ExecutionDownloadError,
    ExecutionDownloadFact, ExecutionDownloadFactKind, ExecutionDownloadReport, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
use super::path_safety::{
    bounded_download_file_label, filesystem_path, safe_download_target_label,
};
use super::promotion::{promotion_backup_path, sweep_stale_promotion_backups};
use super::transfer_failure::{
    finish_execution_error, finish_execution_error_after_temp_discard,
    finish_io_failure_after_temp_discard, record_io_failure_fact_pair,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const DOWNLOAD_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];

#[cfg(test)]
pub(super) async fn download_file_with_client(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExecutionDownloadReport, DownloadError> {
    download_file_with_client_report(client, url, destination, expected)
        .await
        .map_err(ExecutionDownloadError::into_download_error)
}

struct SelectedArtifactDownload<'a> {
    kind: SelectedDownloadArtifactKind,
    client: &'a reqwest::Client,
    url: &'a str,
    destination: &'a Path,
    expected: &'a ExpectedIntegrity,
    fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&'a mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
}

pub(super) struct VerifiedSelectedArtifactDownload<'a> {
    pub(super) kind: SelectedDownloadArtifactKind,
    pub(super) client: &'a reqwest::Client,
    pub(super) url: &'a str,
    pub(super) destination: &'a Path,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) max_bytes: usize,
    pub(super) fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
    pub(super) descriptor_tx: Option<&'a mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
}

pub(super) async fn fetch_verified_selected_artifact_bytes_with_client(
    request: VerifiedSelectedArtifactDownload<'_>,
) -> Result<Arc<[u8]>, DownloadError> {
    let retry_delays = default_download_retry_delays();
    fetch_verified_selected_artifact_bytes_with_retry_delays(request, &retry_delays).await
}

async fn fetch_verified_selected_artifact_bytes_with_retry_delays(
    request: VerifiedSelectedArtifactDownload<'_>,
    retry_delays: &[Duration],
) -> Result<Arc<[u8]>, DownloadError> {
    let VerifiedSelectedArtifactDownload {
        kind,
        client,
        url,
        destination,
        expected,
        max_bytes,
        fact_tx,
        descriptor_tx,
    } = request;
    guard_existing_unsupported_selected_artifact(
        kind,
        destination,
        url,
        expected,
        fact_tx,
        descriptor_tx,
    )
    .await?;
    let Some(expected_sha1) = expected.sha1.as_deref() else {
        emit_selected_metadata_failure(
            kind,
            destination,
            expected,
            fact_tx,
            ExecutionDownloadFactKind::MetadataMissing,
            "sha1",
        );
        return Err(selected_artifact_metadata_error(destination, "missing"));
    };
    if !is_sha1_hex(expected_sha1) {
        emit_selected_metadata_failure(
            kind,
            destination,
            expected,
            fact_tx,
            ExecutionDownloadFactKind::MetadataInvalid,
            "sha1",
        );
        return Err(selected_artifact_metadata_error(destination, "invalid"));
    }

    emit_selected_download_descriptor(descriptor_tx, kind, destination, url, expected);
    emit_selected_artifact_missing_fact_if_absent(fact_tx, kind, destination, expected).await;
    let target = selected_download_target_label(kind, destination);
    let max_bytes = u64::try_from(max_bytes).unwrap_or(u64::MAX);
    let mut next_delay = 0_usize;
    let body = loop {
        match fetch_verified_selected_artifact_bytes_attempt(
            client,
            url,
            destination,
            expected,
            max_bytes,
            fact_tx,
            &target,
        )
        .await
        {
            Ok(body) => break body,
            Err(error) if error.retryable && next_delay < retry_delays.len() => {
                let delay = retry_delays[next_delay];
                next_delay += 1;
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error.error),
        }
    };

    let temp_path = download_temp_path(destination);
    let report = write_launcher_managed_artifact_bytes_to_temp(destination, &temp_path, &body)
        .await
        .map_err(ExecutionDownloadError::into_download_error)?;
    let mut facts = selected_execution_download_facts(kind, destination, &report.facts)
        .into_iter()
        .filter(|fact| fact.kind != ExecutionDownloadFactKind::MetadataMissing)
        .collect::<Vec<_>>();
    facts.extend(metadata_facts(expected, &target));
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::ArtifactVerified,
        &target,
        no_download_fact_fields(),
    ));
    emit_execution_download_facts(fact_tx, &facts);
    Ok(Arc::from(body))
}

#[cfg(test)]
pub(super) async fn fetch_verified_selected_artifact_bytes_with_retry_delays_for_test(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    max_bytes: usize,
    retry_delays: &[Duration],
) -> Result<Arc<[u8]>, DownloadError> {
    fetch_verified_selected_artifact_bytes_with_retry_delays(
        VerifiedSelectedArtifactDownload {
            kind,
            client,
            url,
            destination,
            expected,
            max_bytes,
            fact_tx: None,
            descriptor_tx: None,
        },
        retry_delays,
    )
    .await
}

struct VerifiedSelectedBytesAttemptError {
    error: DownloadError,
    retryable: bool,
}

async fn fetch_verified_selected_artifact_bytes_attempt(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    max_bytes: u64,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    target: &str,
) -> Result<Vec<u8>, VerifiedSelectedBytesAttemptError> {
    let response = client.get(url).send().await.map_err(|error| {
        emit_execution_download_facts(
            fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::NetworkFailure,
                target,
                no_download_fact_fields(),
            )],
        );
        VerifiedSelectedBytesAttemptError {
            error: DownloadError::Request(error),
            retryable: true,
        }
    })?;
    if let Err(error) = response.error_for_status_ref() {
        let status = response.status();
        emit_execution_download_facts(
            fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::ProviderFailure,
                target,
                vec![("status", status.as_u16().to_string())],
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Request(error),
            retryable: is_retryable_provider_status(status.as_u16()),
        });
    }
    let declared_content_length = response.content_length();
    if declared_content_length.is_some_and(|length| length > max_bytes) {
        emit_execution_download_facts(
            fact_tx,
            &[size_mismatch_fact(
                target,
                max_bytes,
                declared_content_length.unwrap_or(0),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} exceeds the safe in-memory size limit",
                bounded_download_file_label(destination)
            )),
            retryable: false,
        });
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            emit_execution_download_facts(
                fact_tx,
                &[execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    target,
                    no_download_fact_fields(),
                )],
            );
            VerifiedSelectedBytesAttemptError {
                error: DownloadError::Request(error),
                retryable: true,
            }
        })?;
        let next_len = body.len().saturating_add(chunk.len());
        if u64::try_from(next_len).unwrap_or(u64::MAX) > max_bytes {
            emit_execution_download_facts(
                fact_tx,
                &[size_mismatch_fact(
                    target,
                    max_bytes,
                    u64::try_from(next_len).unwrap_or(u64::MAX),
                )],
            );
            return Err(VerifiedSelectedBytesAttemptError {
                error: DownloadError::Integrity(format!(
                    "{} exceeds the safe in-memory size limit",
                    bounded_download_file_label(destination)
                )),
                retryable: false,
            });
        }
        body.extend_from_slice(&chunk);
    }
    if declared_content_length.is_some_and(|length| length != body.len() as u64) {
        emit_execution_download_facts(
            fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::Interrupted,
                target,
                no_download_fact_fields(),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} download ended before the declared content length",
                bounded_download_file_label(destination)
            )),
            retryable: true,
        });
    }

    let actual = ActualIntegrity {
        size: body.len() as u64,
        sha1: Some(format!("{:x}", Sha1::digest(&body))),
    };
    if let Err(error) = verify_download_integrity(destination, expected, &actual) {
        emit_execution_download_facts(fact_tx, &[integrity_mismatch_fact(target, &error)]);
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(error.to_string()),
            retryable: false,
        });
    }
    Ok(body)
}

#[derive(Clone, Copy)]
enum DownloadChecksumRequirement {
    Required,
    AllowMissing,
}

impl DownloadChecksumRequirement {
    fn request<'a>(
        self,
        url: &'a str,
        destination: &'a Path,
        expected: &'a ExpectedIntegrity,
    ) -> ExecutionDownloadRequest<'a> {
        match self {
            Self::Required => {
                ExecutionDownloadRequest::launcher_managed(url, destination, expected)
            }
            Self::AllowMissing => {
                ExecutionDownloadRequest::launcher_managed_best_effort(url, destination, expected)
            }
        }
    }
}

pub(super) async fn download_file_with_client_and_fact_sender_allowing_missing_checksum(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<ExecutionDownloadReport, DownloadError> {
    guard_existing_unsupported_selected_artifact(
        kind,
        destination,
        url,
        expected,
        fact_tx,
        descriptor_tx,
    )
    .await?;
    emit_selected_download_descriptor(descriptor_tx, kind, destination, url, expected);
    emit_selected_artifact_missing_fact_if_absent(fact_tx, kind, destination, expected).await;
    let retry_delays = default_download_retry_delays();
    match download_launcher_managed_with_transient_retries(
        client,
        url,
        destination,
        expected,
        DownloadChecksumRequirement::AllowMissing,
        &retry_delays,
    )
    .await
    {
        Ok(report) => {
            if !checksumless_artifact_is_structurally_usable(destination, expected).await? {
                let _ = async_fs::remove_file(filesystem_path(destination).as_ref()).await;
                return Err(checksumless_artifact_structure_error(destination));
            }
            let facts = selected_execution_download_facts(kind, destination, &report.facts);
            emit_execution_download_facts(fact_tx, &facts);
            Ok(report)
        }
        Err(error) => {
            let facts = selected_execution_download_facts(kind, destination, &error.facts);
            emit_execution_download_facts(fact_tx, &facts);
            Err(error.into_download_error())
        }
    }
}

pub(super) async fn ensure_selected_artifact_with_client(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<Option<ExecutionDownloadReport>, DownloadError> {
    match selected_existing_artifact_integrity(kind, destination, expected).await? {
        ExistingArtifactIntegrity::Verified => Ok(None),
        ExistingArtifactIntegrity::MetadataInvalid => {
            emit_selected_metadata_failure(
                kind,
                destination,
                expected,
                fact_tx,
                ExecutionDownloadFactKind::MetadataInvalid,
                "sha1",
            );
            Err(selected_artifact_metadata_error(destination, "invalid"))
        }
        ExistingArtifactIntegrity::MetadataMissing => {
            emit_selected_metadata_failure(
                kind,
                destination,
                expected,
                fact_tx,
                ExecutionDownloadFactKind::MetadataMissing,
                "sha1",
            );
            Err(selected_artifact_metadata_error(destination, "missing"))
        }
        ExistingArtifactIntegrity::Corrupt(error) => download_selected_artifact_with_client(
            SelectedArtifactDownload {
                kind,
                client,
                url,
                destination,
                expected,
                fact_tx,
                descriptor_tx,
            },
            Some(error),
        )
        .await
        .map(Some),
        ExistingArtifactIntegrity::UnsupportedExisting => {
            emit_unsupported_selected_artifact(
                kind,
                destination,
                url,
                expected,
                fact_tx,
                descriptor_tx,
            );
            Err(unsupported_selected_artifact_error(destination))
        }
        ExistingArtifactIntegrity::Missing => download_selected_artifact_with_client(
            SelectedArtifactDownload {
                kind,
                client,
                url,
                destination,
                expected,
                fact_tx,
                descriptor_tx,
            },
            None,
        )
        .await
        .map(Some),
    }
}

pub(super) async fn ensure_selected_artifact_with_client_allowing_missing_checksum(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<Option<ExecutionDownloadReport>, DownloadError> {
    if expected
        .sha1
        .as_deref()
        .is_some_and(|sha1| !is_sha1_hex(sha1))
    {
        emit_selected_metadata_failure(
            kind,
            destination,
            expected,
            fact_tx,
            ExecutionDownloadFactKind::MetadataInvalid,
            "sha1",
        );
        return Err(selected_artifact_metadata_error(destination, "invalid"));
    }

    if expected.sha1.is_some() {
        return ensure_selected_artifact_with_client(
            kind,
            client,
            url,
            destination,
            expected,
            fact_tx,
            descriptor_tx,
        )
        .await;
    }

    match async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_file() => {
            emit_unsupported_selected_artifact(
                kind,
                destination,
                url,
                expected,
                fact_tx,
                descriptor_tx,
            );
            return Err(unsupported_selected_artifact_error(destination));
        }
        Ok(metadata) => {
            if expected
                .size
                .is_none_or(|expected_size| metadata.len() == expected_size)
                && checksumless_artifact_is_structurally_usable(destination, expected).await?
            {
                return Ok(None);
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(DownloadError::FileOperation(error)),
    }

    download_file_with_client_and_fact_sender_allowing_missing_checksum(
        kind,
        client,
        url,
        destination,
        expected,
        fact_tx,
        descriptor_tx,
    )
    .await
    .map(Some)
}

async fn checksumless_artifact_is_structurally_usable(
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    if expected.sha1.is_some() {
        return Ok(true);
    }
    let metadata = match async_fs::metadata(filesystem_path(destination).as_ref()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(DownloadError::FileOperation(error)),
    };
    if !metadata.is_file() || metadata.len() == 0 {
        return Ok(false);
    }
    if destination
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("jar"))
    {
        return checksumless_jar_is_readable_async(destination.to_path_buf()).await;
    }
    Ok(true)
}

async fn selected_existing_artifact_integrity(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExistingArtifactIntegrity, DownloadError> {
    if kind == SelectedDownloadArtifactKind::AssetObject {
        existing_content_addressed_asset_integrity(destination, expected).await
    } else {
        existing_artifact_integrity(destination, expected).await
    }
}

async fn guard_existing_unsupported_selected_artifact(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    url: &str,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<(), DownloadError> {
    let Ok(metadata) = async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await
    else {
        return Ok(());
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        emit_unsupported_selected_artifact(
            kind,
            destination,
            url,
            expected,
            fact_tx,
            descriptor_tx,
        );
        return Err(unsupported_selected_artifact_error(destination));
    }
    Ok(())
}

async fn download_selected_artifact_with_client(
    request: SelectedArtifactDownload<'_>,
    existing_corrupt: Option<DownloadIntegrityError>,
) -> Result<ExecutionDownloadReport, DownloadError> {
    let SelectedArtifactDownload {
        kind,
        client,
        url,
        destination,
        expected,
        fact_tx,
        descriptor_tx,
    } = request;
    emit_selected_download_descriptor(descriptor_tx, kind, destination, url, expected);
    if let Some(error) = existing_corrupt.as_ref() {
        emit_existing_corrupt_selected_artifact_fact(kind, destination, fact_tx, error);
    }
    emit_selected_artifact_missing_fact_if_absent(fact_tx, kind, destination, expected).await;
    match download_file_with_client_report(client, url, destination, expected).await {
        Ok(report) => {
            let mut facts = selected_execution_download_facts(kind, destination, &report.facts);
            if existing_corrupt.is_some() {
                let target = selected_download_target_label(kind, destination);
                facts.push(corrupt_artifact_replaced_fact(&target));
            }
            emit_execution_download_facts(fact_tx, &facts);
            Ok(report)
        }
        Err(error) => {
            let facts = selected_execution_download_facts(kind, destination, &error.facts);
            emit_execution_download_facts(fact_tx, &facts);
            Err(error.into_download_error())
        }
    }
}

fn emit_existing_corrupt_selected_artifact_fact(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    error: &DownloadIntegrityError,
) {
    if let Some(fact_tx) = fact_tx {
        let target = selected_download_target_label(kind, destination);
        let _ = fact_tx.send(integrity_mismatch_fact(&target, error));
    }
}

fn corrupt_artifact_replaced_fact(target: &str) -> ExecutionDownloadFact {
    execution_download_fact(
        ExecutionDownloadFactKind::Promoted,
        target,
        vec![("replaced", "corrupt")],
    )
}

fn emit_unsupported_selected_artifact(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    url: &str,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) {
    emit_selected_download_descriptor(descriptor_tx, kind, destination, url, expected);
    if let Some(fact_tx) = fact_tx {
        let target = selected_download_target_label(kind, destination);
        let _ = fact_tx.send(execution_download_fact(
            ExecutionDownloadFactKind::OwnershipRefused,
            &target,
            no_download_fact_fields(),
        ));
    }
}

fn emit_selected_metadata_failure(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    fact_kind: ExecutionDownloadFactKind,
    field: &str,
) {
    let Some(fact_tx) = fact_tx else {
        return;
    };
    let target = selected_download_target_label(kind, destination);
    let mut facts = metadata_facts(expected, &target);
    if !facts.iter().any(|fact| fact.kind == fact_kind) {
        facts.push(execution_download_fact(
            fact_kind,
            &target,
            vec![("field", field)],
        ));
    }
    emit_execution_download_facts(Some(fact_tx), &facts);
}

fn unsupported_selected_artifact_error(destination: &Path) -> DownloadError {
    DownloadError::Integrity(format!(
        "{} target is not a regular launcher-managed artifact",
        bounded_download_file_label(destination)
    ))
}

fn selected_artifact_metadata_error(destination: &Path, status: &str) -> DownloadError {
    DownloadError::Integrity(format!(
        "{} integrity metadata is {status}",
        bounded_download_file_label(destination)
    ))
}

fn checksumless_artifact_structure_error(destination: &Path) -> DownloadError {
    DownloadError::Integrity(format!(
        "{} could not be validated as a usable checksumless artifact",
        bounded_download_file_label(destination)
    ))
}

async fn emit_selected_artifact_missing_fact_if_absent(
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    expected: &ExpectedIntegrity,
) {
    let Some(fact_tx) = fact_tx else {
        return;
    };
    if !matches!(
        async_fs::try_exists(filesystem_path(destination).as_ref()).await,
        Ok(false)
    ) {
        return;
    }
    let Some(fact) = selected_artifact_missing_fact(kind, destination, expected) else {
        return;
    };
    let _ = fact_tx.send(fact);
}

fn selected_execution_download_facts(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    facts: &[ExecutionDownloadFact],
) -> Vec<ExecutionDownloadFact> {
    let target = selected_download_target_label(kind, destination);
    facts
        .iter()
        .cloned()
        .map(|mut fact| {
            fact.target = target.clone();
            fact
        })
        .collect()
}

pub async fn download_file_with_client_report(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let retry_delays = default_download_retry_delays();
    download_launcher_managed_with_transient_retries(
        client,
        url,
        destination,
        expected,
        DownloadChecksumRequirement::Required,
        &retry_delays,
    )
    .await
}

#[cfg(test)]
pub(super) async fn download_file_with_client_report_with_retry_delays(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    retry_delays: &[Duration],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    download_launcher_managed_with_transient_retries(
        client,
        url,
        destination,
        expected,
        DownloadChecksumRequirement::Required,
        retry_delays,
    )
    .await
}

fn default_download_retry_delays() -> [Duration; 3] {
    DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis)
}

async fn download_launcher_managed_with_transient_retries(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    checksum_requirement: DownloadChecksumRequirement,
    retry_delays: &[Duration],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let mut next_delay = 0_usize;
    loop {
        match execute_download_to_temp(
            client,
            checksum_requirement.request(url, destination, expected),
        )
        .await
        {
            Ok(report) => return Ok(report),
            Err(error)
                if next_delay < retry_delays.len()
                    && execution_download_error_is_retryable(&error) =>
            {
                let delay = retry_delays[next_delay];
                next_delay += 1;
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error),
        }
    }
}

fn execution_download_error_is_retryable(error: &ExecutionDownloadError) -> bool {
    match error.kind {
        ExecutionDownloadFactKind::NetworkFailure | ExecutionDownloadFactKind::Interrupted => true,
        ExecutionDownloadFactKind::ProviderFailure => {
            provider_failure_status(error).is_some_and(is_retryable_provider_status)
        }
        ExecutionDownloadFactKind::ArtifactMissing
        | ExecutionDownloadFactKind::ArtifactVerified
        | ExecutionDownloadFactKind::ChecksumMismatch
        | ExecutionDownloadFactKind::MetadataInvalid
        | ExecutionDownloadFactKind::MetadataMissing
        | ExecutionDownloadFactKind::OwnershipRefused
        | ExecutionDownloadFactKind::PermissionFailure
        | ExecutionDownloadFactKind::PromoteFailed
        | ExecutionDownloadFactKind::SizeMismatch
        | ExecutionDownloadFactKind::TempDiscarded
        | ExecutionDownloadFactKind::TempWriteFailed
        | ExecutionDownloadFactKind::WrittenToTemp
        | ExecutionDownloadFactKind::Promoted => false,
    }
}

fn provider_failure_status(error: &ExecutionDownloadError) -> Option<u16> {
    if let DownloadError::Request(request_error) = &error.error
        && let Some(status) = request_error.status()
    {
        return Some(status.as_u16());
    }

    error
        .facts
        .iter()
        .filter(|fact| fact.kind == ExecutionDownloadFactKind::ProviderFailure)
        .flat_map(|fact| fact.fields.iter())
        .find_map(|(key, value)| {
            (key == "status")
                .then(|| value.parse::<u16>().ok())
                .flatten()
        })
}

fn is_retryable_provider_status(status: u16) -> bool {
    status == 429 || (500..=599).contains(&status)
}

pub(super) fn download_temp_path(destination: &Path) -> PathBuf {
    let mut name = destination
        .file_name()
        .unwrap_or_else(|| OsStr::new("download"))
        .to_os_string();
    name.push(".axial-tmp");
    destination.with_file_name(name)
}

pub(crate) async fn write_launcher_managed_artifact_bytes_to_temp(
    destination: &Path,
    temp_path: &Path,
    bytes: &[u8],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let target = safe_download_target_label(destination);
    let expected = ExpectedIntegrity::default();
    let mut facts = metadata_facts(&expected, &target);

    if let Some(parent) = destination.parent()
        && let Err(error) = async_fs::create_dir_all(filesystem_path(parent).as_ref()).await
    {
        let kind = record_io_failure_fact_pair(
            &mut facts,
            &target,
            error.kind(),
            ExecutionDownloadFactKind::TempWriteFailed,
        );
        return Err(finish_execution_error(
            &mut facts,
            kind,
            DownloadError::FileOperation(error),
        ));
    }

    discard_download_temp(temp_path, &target, &mut facts).await;
    let mut output = match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(filesystem_path(temp_path).as_ref())
        .await
    {
        Ok(output) => output,
        Err(error) => {
            record_io_failure_fact_pair(
                &mut facts,
                &target,
                error.kind(),
                ExecutionDownloadFactKind::TempWriteFailed,
            );
            return Err(finish_execution_error(
                &mut facts,
                ExecutionDownloadFactKind::TempWriteFailed,
                DownloadError::FileOperation(error),
            ));
        }
    };

    if let Err(error) = output.write_all(bytes).await {
        drop(output);
        return Err(finish_io_failure_after_temp_discard(
            temp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::TempWriteFailed,
            ExecutionDownloadFactKind::TempWriteFailed,
            error,
        )
        .await);
    }
    if let Err(error) = output.flush().await {
        drop(output);
        return Err(finish_io_failure_after_temp_discard(
            temp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::TempWriteFailed,
            ExecutionDownloadFactKind::TempWriteFailed,
            error,
        )
        .await);
    }
    drop(output);

    let written = bytes.len() as u64;
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        &target,
        vec![("bytes", written.to_string())],
    ));

    if let Err(error) = promote_launcher_managed_artifact_temp_once(temp_path, destination).await {
        return Err(finish_io_failure_after_temp_discard(
            temp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::PromoteFailed,
            ExecutionDownloadFactKind::PromoteFailed,
            error,
        )
        .await);
    }
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::Promoted,
        &target,
        no_download_fact_fields(),
    ));

    Ok(ExecutionDownloadReport {
        target,
        bytes_written: written,
        facts,
    })
}

pub(super) async fn execute_download_to_temp(
    client: &reqwest::Client,
    request: ExecutionDownloadRequest<'_>,
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let target = safe_download_target_label(request.destination);
    let mut facts = metadata_facts(request.expected, &target);
    if !request.ownership.allows_managed_mutation() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::OwnershipRefused,
            &target,
            no_download_fact_fields(),
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::OwnershipRefused,
            facts,
            DownloadError::FileOperation(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "download target ownership is not launcher managed",
            )),
        ));
    }
    if let Some(expected_sha1) = request.expected.sha1.as_deref()
        && !is_sha1_hex(expected_sha1)
    {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            &target,
            vec![("field", "sha1")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataInvalid,
            facts,
            DownloadError::Integrity(format!(
                "{} integrity metadata is invalid",
                bounded_download_file_label(request.destination)
            )),
        ));
    }
    if request.require_checksum && request.expected.sha1.is_none() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataMissing,
            &target,
            vec![("field", "sha1")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataMissing,
            facts,
            DownloadError::Integrity(format!(
                "{} integrity metadata is missing",
                bounded_download_file_label(request.destination)
            )),
        ));
    }

    if let Some(parent) = request.destination.parent()
        && let Err(error) = async_fs::create_dir_all(filesystem_path(parent).as_ref()).await
    {
        let kind = record_io_failure_fact_pair(
            &mut facts,
            &target,
            error.kind(),
            ExecutionDownloadFactKind::TempWriteFailed,
        );
        return Err(finish_execution_error(
            &mut facts,
            kind,
            DownloadError::FileOperation(error),
        ));
    }

    let tmp_path = download_temp_path(request.destination);
    discard_download_temp(&tmp_path, &target, &mut facts).await;
    let response = match client.get(request.url).send().await {
        Ok(response) => response,
        Err(error) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::NetworkFailure,
                &target,
                no_download_fact_fields(),
            ));
            return Err(execution_download_error(
                ExecutionDownloadFactKind::NetworkFailure,
                facts,
                DownloadError::Request(error),
            ));
        }
    };

    if let Err(error) = response.error_for_status_ref() {
        let status = response.status().as_u16().to_string();
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::ProviderFailure,
            &target,
            vec![("status", status.as_str())],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::ProviderFailure,
            facts,
            DownloadError::Request(error),
        ));
    }

    let declared_content_length = response.content_length();
    if let Some(expected_size) = request.expected.size
        && let Some(content_length) = declared_content_length
        && content_length > expected_size
    {
        facts.push(size_mismatch_fact(&target, expected_size, content_length));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::SizeMismatch,
            facts,
            download_size_mismatch(request.destination, expected_size, content_length),
        ));
    }

    let mut output = match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(filesystem_path(&tmp_path).as_ref())
        .await
    {
        Ok(output) => output,
        Err(error) => {
            record_io_failure_fact_pair(
                &mut facts,
                &target,
                error.kind(),
                ExecutionDownloadFactKind::TempWriteFailed,
            );
            return Err(finish_execution_error(
                &mut facts,
                ExecutionDownloadFactKind::TempWriteFailed,
                DownloadError::FileOperation(error),
            ));
        }
    };
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::Interrupted,
                    &target,
                    no_download_fact_fields(),
                ));
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    &target,
                    no_download_fact_fields(),
                ));
                drop(output);
                return Err(finish_execution_error_after_temp_discard(
                    &tmp_path,
                    &target,
                    &mut facts,
                    ExecutionDownloadFactKind::NetworkFailure,
                    DownloadError::Request(error),
                )
                .await);
            }
        };
        let next_written = written.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = request.expected.size
            && next_written > expected_size
        {
            facts.push(size_mismatch_fact(&target, expected_size, next_written));
            drop(output);
            return Err(finish_execution_error_after_temp_discard(
                &tmp_path,
                &target,
                &mut facts,
                ExecutionDownloadFactKind::SizeMismatch,
                download_size_mismatch(request.destination, expected_size, next_written),
            )
            .await);
        }
        hasher.update(&chunk);
        if let Err(error) = output.write_all(&chunk).await {
            drop(output);
            return Err(finish_io_failure_after_temp_discard(
                &tmp_path,
                &target,
                &mut facts,
                ExecutionDownloadFactKind::TempWriteFailed,
                ExecutionDownloadFactKind::TempWriteFailed,
                error,
            )
            .await);
        }
        written = next_written;
    }
    if let Some(content_length) = declared_content_length
        && written != content_length
    {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::Interrupted,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(size_mismatch_fact(&target, content_length, written));
        drop(output);
        return Err(finish_execution_error_after_temp_discard(
            &tmp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::Interrupted,
            DownloadError::Integrity(format!(
                "{} download ended before the declared content length",
                bounded_download_file_label(request.destination)
            )),
        )
        .await);
    }
    if let Err(error) = output.flush().await {
        drop(output);
        return Err(finish_io_failure_after_temp_discard(
            &tmp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::TempWriteFailed,
            ExecutionDownloadFactKind::TempWriteFailed,
            error,
        )
        .await);
    }
    drop(output);
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        &target,
        vec![("bytes", written.to_string())],
    ));

    let actual = ActualIntegrity {
        size: written,
        sha1: Some(format!("{:x}", hasher.finalize())),
    };
    if let Err(error) = verify_download_integrity(request.destination, request.expected, &actual) {
        let error_kind = match &error {
            DownloadIntegrityError::SizeMismatch {
                expected, actual, ..
            } => {
                facts.push(size_mismatch_fact(&target, *expected, *actual));
                ExecutionDownloadFactKind::SizeMismatch
            }
            DownloadIntegrityError::Sha1Mismatch { .. }
            | DownloadIntegrityError::MissingSha1 { .. } => {
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::ChecksumMismatch,
                    &target,
                    vec![("algorithm", "sha1")],
                ));
                ExecutionDownloadFactKind::ChecksumMismatch
            }
        };
        return Err(finish_execution_error_after_temp_discard(
            &tmp_path,
            &target,
            &mut facts,
            error_kind,
            DownloadError::Integrity(error.to_string()),
        )
        .await);
    }
    if request.expected.has_evidence() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::ArtifactVerified,
            &target,
            no_download_fact_fields(),
        ));
    }

    if let Err(error) =
        promote_launcher_managed_artifact_temp_once(&tmp_path, request.destination).await
    {
        return Err(finish_io_failure_after_temp_discard(
            &tmp_path,
            &target,
            &mut facts,
            ExecutionDownloadFactKind::PromoteFailed,
            ExecutionDownloadFactKind::PromoteFailed,
            error,
        )
        .await);
    }
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::Promoted,
        &target,
        no_download_fact_fields(),
    ));

    Ok(ExecutionDownloadReport {
        target,
        bytes_written: written,
        facts,
    })
}

pub(super) async fn discard_download_temp(
    temp_path: &Path,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
) {
    match async_fs::symlink_metadata(filesystem_path(temp_path).as_ref()).await {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => return,
        Err(_) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                Vec::<(&str, &str)>::new(),
            ));
            return;
        }
    }

    match remove_stale_download_temp(temp_path).await {
        Ok(()) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempDiscarded,
                target,
                Vec::<(&str, &str)>::new(),
            ));
        }
        Err(DownloadError::FileOperation(error)) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                Vec::<(&str, &str)>::new(),
            ));
        }
    }
}

pub(crate) async fn promote_launcher_managed_artifact_temp_once(
    temp_path: &Path,
    destination: &Path,
) -> io::Result<()> {
    sweep_stale_promotion_backups(destination).await?;
    match async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            let backup_path = promotion_backup_path(destination);
            async_fs::rename(
                filesystem_path(destination).as_ref(),
                filesystem_path(&backup_path).as_ref(),
            )
            .await?;
            match async_fs::rename(
                filesystem_path(temp_path).as_ref(),
                filesystem_path(destination).as_ref(),
            )
            .await
            {
                Ok(()) => {
                    let _ = async_fs::remove_file(filesystem_path(&backup_path).as_ref()).await;
                    Ok(())
                }
                Err(error) => {
                    let restore_result = async_fs::rename(
                        filesystem_path(&backup_path).as_ref(),
                        filesystem_path(destination).as_ref(),
                    )
                    .await;
                    restore_result?;
                    Err(error)
                }
            }
        }
        Ok(_) | Err(_) => {
            async_fs::rename(
                filesystem_path(temp_path).as_ref(),
                filesystem_path(destination).as_ref(),
            )
            .await
        }
    }
}

pub(super) async fn remove_stale_download_temp(temp_path: &Path) -> Result<(), DownloadError> {
    let metadata = match async_fs::symlink_metadata(filesystem_path(temp_path).as_ref()).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(DownloadError::FileOperation(error)),
    };
    let file_type = metadata.file_type();
    let result = if metadata.is_dir() && !file_type.is_symlink() {
        async_fs::remove_dir_all(filesystem_path(temp_path).as_ref()).await
    } else {
        async_fs::remove_file(filesystem_path(temp_path).as_ref()).await
    };

    result.map_err(DownloadError::FileOperation)
}
