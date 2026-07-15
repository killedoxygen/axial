use super::facts::{
    emit_execution_download_facts, execution_download_fact, integrity_mismatch_fact,
    metadata_facts, no_download_fact_fields, size_mismatch_fact,
};
use super::integrity::is_sha1_hex;
use super::model::{
    DownloadError, DownloadIntegrityError, ExecutionDownloadError, ExecutionDownloadFact,
    ExecutionDownloadFactKind, ExecutionDownloadReport, ExpectedIntegrity,
    SelectedDownloadArtifactKind,
};
use super::path_safety::{filesystem_path, safe_download_target_label};
use super::promotion::{promotion_backup_path, sweep_stale_promotion_backups};
use super::transfer_failure::{
    finish_execution_error, finish_io_failure_after_temp_discard, record_io_failure_fact_pair,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::io;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

const DOWNLOAD_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];

pub(crate) struct AuthenticatedSelectedArtifactSource {
    bytes: Arc<[u8]>,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: SelectedDownloadArtifactKind,
    provider_url: String,
    logical_identity: String,
    expected: ExpectedIntegrity,
}

pub(crate) struct AuthenticatedSelectedArtifactVersionBundleParts {
    pub(crate) bytes: Arc<[u8]>,
    pub(crate) observed_size: u64,
    pub(crate) observed_sha1: [u8; 20],
    pub(crate) kind: SelectedDownloadArtifactKind,
    pub(crate) logical_identity: String,
    pub(crate) expected: ExpectedIntegrity,
}

impl AuthenticatedSelectedArtifactSource {
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub(crate) fn kind(&self) -> SelectedDownloadArtifactKind {
        self.kind
    }

    pub(crate) fn provider_url(&self) -> &str {
        &self.provider_url
    }

    pub(crate) fn logical_identity(&self) -> &str {
        &self.logical_identity
    }

    pub(crate) fn expected(&self) -> &ExpectedIntegrity {
        &self.expected
    }

    pub(crate) fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.bytes)
    }

    pub(crate) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    pub(crate) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    pub(crate) fn into_version_bundle_parts(
        self,
    ) -> AuthenticatedSelectedArtifactVersionBundleParts {
        AuthenticatedSelectedArtifactVersionBundleParts {
            bytes: self.bytes,
            observed_size: self.observed_size,
            observed_sha1: self.observed_sha1,
            kind: self.kind,
            logical_identity: self.logical_identity,
            expected: self.expected,
        }
    }
}

pub(super) struct SelectedArtifactSourceRequest<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) kind: SelectedDownloadArtifactKind,
    pub(super) url: &'a str,
    pub(super) logical_identity: &'a str,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) max_bytes: usize,
    pub(super) target: &'a str,
    pub(super) fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
}

pub(super) async fn acquire_authenticated_selected_artifact_source(
    request: SelectedArtifactSourceRequest<'_>,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let retry_delays = default_download_retry_delays();
    acquire_authenticated_selected_artifact_source_with_retry_delays(request, &retry_delays).await
}

async fn acquire_authenticated_selected_artifact_source_with_retry_delays(
    request: SelectedArtifactSourceRequest<'_>,
    retry_delays: &[Duration],
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    let Some(expected_sha1) = request.expected.sha1.as_deref() else {
        emit_source_metadata_failure(
            request.expected,
            request.target,
            request.fact_tx,
            ExecutionDownloadFactKind::MetadataMissing,
            "sha1",
        );
        return Err(source_artifact_metadata_error(request.target, "missing"));
    };
    if !is_sha1_hex(expected_sha1) {
        emit_source_metadata_failure(
            request.expected,
            request.target,
            request.fact_tx,
            ExecutionDownloadFactKind::MetadataInvalid,
            "sha1",
        );
        return Err(source_artifact_metadata_error(request.target, "invalid"));
    }

    let max_bytes = u64::try_from(request.max_bytes).unwrap_or(u64::MAX);
    let mut next_delay = 0_usize;
    loop {
        match acquire_authenticated_selected_artifact_source_attempt(&request, max_bytes).await {
            Ok(source) => return Ok(source),
            Err(error) if error.retryable && next_delay < retry_delays.len() => {
                let delay = retry_delays[next_delay];
                next_delay += 1;
                tokio::time::sleep(delay).await;
            }
            Err(error) => return Err(error.error),
        }
    }
}

#[cfg(test)]
pub(super) async fn acquire_authenticated_selected_artifact_source_with_retry_delays_for_test(
    client: &reqwest::Client,
    url: &str,
    expected: &ExpectedIntegrity,
    max_bytes: usize,
    target: &str,
    retry_delays: &[Duration],
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    acquire_authenticated_selected_artifact_source_with_retry_delays(
        SelectedArtifactSourceRequest {
            client,
            kind: SelectedDownloadArtifactKind::VersionJson,
            url,
            logical_identity: target,
            expected,
            max_bytes,
            target,
            fact_tx: None,
        },
        retry_delays,
    )
    .await
}

struct VerifiedSelectedBytesAttemptError {
    error: DownloadError,
    retryable: bool,
}

async fn acquire_authenticated_selected_artifact_source_attempt(
    request: &SelectedArtifactSourceRequest<'_>,
    max_bytes: u64,
) -> Result<AuthenticatedSelectedArtifactSource, VerifiedSelectedBytesAttemptError> {
    let response = request
        .client
        .get(request.url)
        .send()
        .await
        .map_err(|error| {
            emit_execution_download_facts(
                request.fact_tx,
                &[execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    request.target,
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
            request.fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::ProviderFailure,
                request.target,
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
            request.fact_tx,
            &[size_mismatch_fact(
                request.target,
                max_bytes,
                declared_content_length.unwrap_or(0),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} exceeds the safe in-memory size limit",
                request.target
            )),
            retryable: false,
        });
    }

    let mut body = Vec::new();
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|error| {
            emit_execution_download_facts(
                request.fact_tx,
                &[execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    request.target,
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
                request.fact_tx,
                &[size_mismatch_fact(
                    request.target,
                    max_bytes,
                    u64::try_from(next_len).unwrap_or(u64::MAX),
                )],
            );
            return Err(VerifiedSelectedBytesAttemptError {
                error: DownloadError::Integrity(format!(
                    "{} exceeds the safe in-memory size limit",
                    request.target
                )),
                retryable: false,
            });
        }
        body.extend_from_slice(&chunk);
    }
    if declared_content_length.is_some_and(|length| length != body.len() as u64) {
        emit_execution_download_facts(
            request.fact_tx,
            &[execution_download_fact(
                ExecutionDownloadFactKind::Interrupted,
                request.target,
                no_download_fact_fields(),
            )],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(format!(
                "{} download ended before the declared content length",
                request.target
            )),
            retryable: true,
        });
    }

    let observed_size = body.len() as u64;
    let observed_sha1: [u8; 20] = Sha1::digest(&body).into();
    if let Err(error) = verify_source_integrity(
        request.target,
        request.expected,
        observed_size,
        &observed_sha1,
    ) {
        emit_execution_download_facts(
            request.fact_tx,
            &[integrity_mismatch_fact(request.target, &error)],
        );
        return Err(VerifiedSelectedBytesAttemptError {
            error: DownloadError::Integrity(error.to_string()),
            retryable: false,
        });
    }
    Ok(AuthenticatedSelectedArtifactSource {
        bytes: Arc::from(body),
        observed_size,
        observed_sha1,
        kind: request.kind,
        provider_url: request.url.to_string(),
        logical_identity: request.logical_identity.to_string(),
        expected: request.expected.clone(),
    })
}

fn verify_source_integrity(
    target: &str,
    expected: &ExpectedIntegrity,
    observed_size: u64,
    observed_sha1: &[u8; 20],
) -> Result<(), DownloadIntegrityError> {
    if let Some(expected_size) = expected.size
        && observed_size != expected_size
    {
        return Err(DownloadIntegrityError::SizeMismatch {
            file: target.to_string(),
            expected: expected_size,
            actual: observed_size,
        });
    }
    let expected_sha1 = expected
        .sha1
        .as_deref()
        .expect("source acquisition validates expected sha1 first");
    let actual_sha1 = observed_sha1
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
        return Err(DownloadIntegrityError::Sha1Mismatch {
            file: target.to_string(),
            expected: expected_sha1.to_string(),
            actual: actual_sha1,
        });
    }
    Ok(())
}

fn emit_source_metadata_failure(
    expected: &ExpectedIntegrity,
    target: &str,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    fact_kind: ExecutionDownloadFactKind,
    field: &str,
) {
    let Some(fact_tx) = fact_tx else {
        return;
    };
    let mut facts = metadata_facts(expected, target);
    if !facts.iter().any(|fact| fact.kind == fact_kind) {
        facts.push(execution_download_fact(
            fact_kind,
            target,
            vec![("field", field)],
        ));
    }
    emit_execution_download_facts(Some(fact_tx), &facts);
}

fn source_artifact_metadata_error(target: &str, status: &str) -> DownloadError {
    DownloadError::Integrity(format!("{target} integrity metadata is {status}"))
}

fn default_download_retry_delays() -> [Duration; 3] {
    DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis)
}

fn is_retryable_provider_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
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
