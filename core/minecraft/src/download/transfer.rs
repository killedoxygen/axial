use super::facts::{
    ExecutionDownloadRequest, emit_execution_download_facts, emit_selected_download_descriptor,
    execution_download_error, execution_download_fact, integrity_mismatch_fact,
    io_execution_fact_kind, metadata_facts, no_download_fact_fields,
    selected_artifact_missing_fact, selected_download_target_label, size_mismatch_fact,
};
use super::integrity::{
    ExistingArtifactIntegrity, download_size_mismatch, existing_artifact_integrity, is_sha1_hex,
    verify_download_integrity,
};
use super::model::{
    ActualIntegrity, DownloadError, DownloadIntegrityError, ExecutionDownloadError,
    ExecutionDownloadFact, ExecutionDownloadFactKind, ExecutionDownloadReport, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
use super::path_safety::{bounded_download_file_label, safe_download_target_label};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

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

pub(super) async fn download_file_with_client_and_fact_sender(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<ExecutionDownloadReport, DownloadError> {
    guard_existing_unsafe_selected_artifact(
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
    match download_file_with_client_report(client, url, destination, expected).await {
        Ok(report) => {
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
    match execute_download_to_temp(
        client,
        ExecutionDownloadRequest::launcher_managed_best_effort(url, destination, expected),
    )
    .await
    {
        Ok(report) => {
            if !checksumless_artifact_is_structurally_usable(destination, expected).await? {
                let _ = async_fs::remove_file(destination).await;
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
    match existing_artifact_integrity(destination, expected).await? {
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
        ExistingArtifactIntegrity::Corrupt(error) => {
            emit_existing_corrupt_selected_artifact(
                kind,
                destination,
                url,
                expected,
                fact_tx,
                descriptor_tx,
                &error,
            );
            Err(DownloadError::Integrity(error.to_string()))
        }
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
        ExistingArtifactIntegrity::Missing => download_file_with_client_and_fact_sender(
            kind,
            client,
            url,
            destination,
            expected,
            fact_tx,
            descriptor_tx,
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

    match async_fs::symlink_metadata(destination).await {
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
    let metadata = match async_fs::metadata(destination).await {
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
        return checksumless_jar_is_readable(destination.to_path_buf()).await;
    }
    Ok(true)
}

async fn checksumless_jar_is_readable(destination: PathBuf) -> Result<bool, DownloadError> {
    tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&destination)?;
        let Ok(mut archive) = zip::ZipArchive::new(file) else {
            return Ok(false);
        };
        for index in 0..archive.len() {
            let Ok(entry) = archive.by_index(index) else {
                return Ok(false);
            };
            if !entry.is_dir() {
                return Ok(true);
            }
        }
        Ok(false)
    })
    .await
    .map_err(|error| DownloadError::FileOperation(io::Error::other(error.to_string())))?
    .map_err(DownloadError::FileOperation)
}

async fn guard_existing_unsafe_selected_artifact(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    url: &str,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<(), DownloadError> {
    match existing_artifact_integrity(destination, expected).await? {
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
        ExistingArtifactIntegrity::Corrupt(error) => {
            emit_existing_corrupt_selected_artifact(
                kind,
                destination,
                url,
                expected,
                fact_tx,
                descriptor_tx,
                &error,
            );
            Err(DownloadError::Integrity(error.to_string()))
        }
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
        ExistingArtifactIntegrity::Missing | ExistingArtifactIntegrity::Verified => Ok(()),
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
    let Ok(metadata) = async_fs::symlink_metadata(destination).await else {
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

fn emit_existing_corrupt_selected_artifact(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    url: &str,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    error: &DownloadIntegrityError,
) {
    emit_selected_download_descriptor(descriptor_tx, kind, destination, url, expected);
    if let Some(fact_tx) = fact_tx {
        let target = selected_download_target_label(kind, destination);
        let _ = fact_tx.send(integrity_mismatch_fact(&target, error));
    }
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
    if !matches!(async_fs::try_exists(destination).await, Ok(false)) {
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
    let mut last_error: Option<DownloadError> = None;
    for attempt in 0..3 {
        match execute_download_to_temp(
            client,
            ExecutionDownloadRequest::launcher_managed(url, destination, expected),
        )
        .await
        {
            Ok(report) => return Ok(report),
            Err(error) => {
                if attempt == 2 {
                    return Err(error);
                }
                last_error = Some(error.into_download_error());
                tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
            }
        }
    }
    Err(execution_download_error(
        ExecutionDownloadFactKind::NetworkFailure,
        Vec::new(),
        last_error.unwrap_or_else(|| DownloadError::ResolveManifest("download failed".to_string())),
    ))
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
        && let Err(error) = async_fs::create_dir_all(parent).await
    {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            &target,
            no_download_fact_fields(),
        ));
        return Err(execution_download_error(
            kind,
            facts,
            DownloadError::FileOperation(error),
        ));
    }

    discard_download_temp(temp_path, &target, &mut facts).await;
    let mut output = match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(temp_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let kind = io_execution_fact_kind(error.kind());
            facts.push(execution_download_fact(
                kind,
                &target,
                no_download_fact_fields(),
            ));
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                &target,
                no_download_fact_fields(),
            ));
            return Err(execution_download_error(
                ExecutionDownloadFactKind::TempWriteFailed,
                facts,
                DownloadError::FileOperation(error),
            ));
        }
    };

    if let Err(error) = output.write_all(bytes).await {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            &target,
            no_download_fact_fields(),
        ));
        drop(output);
        discard_download_temp(temp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::TempWriteFailed,
            facts,
            DownloadError::FileOperation(error),
        ));
    }
    if let Err(error) = output.flush().await {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            &target,
            no_download_fact_fields(),
        ));
        drop(output);
        discard_download_temp(temp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::TempWriteFailed,
            facts,
            DownloadError::FileOperation(error),
        ));
    }
    drop(output);

    let written = bytes.len() as u64;
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        &target,
        vec![("bytes", written.to_string())],
    ));

    if let Err(error) = promote_launcher_managed_artifact_temp_once(temp_path, destination).await {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::PromoteFailed,
            &target,
            no_download_fact_fields(),
        ));
        discard_download_temp(temp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::PromoteFailed,
            facts,
            DownloadError::FileOperation(error),
        ));
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
        && let Err(error) = async_fs::create_dir_all(parent).await
    {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            &target,
            no_download_fact_fields(),
        ));
        return Err(execution_download_error(
            kind,
            facts,
            DownloadError::FileOperation(error),
        ));
    }

    let tmp_path = request.destination.with_extension("tmp");
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
        .open(&tmp_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            let kind = io_execution_fact_kind(error.kind());
            facts.push(execution_download_fact(
                kind,
                &target,
                no_download_fact_fields(),
            ));
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                &target,
                no_download_fact_fields(),
            ));
            return Err(execution_download_error(
                ExecutionDownloadFactKind::TempWriteFailed,
                facts,
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
                discard_download_temp(&tmp_path, &target, &mut facts).await;
                return Err(execution_download_error(
                    ExecutionDownloadFactKind::NetworkFailure,
                    facts,
                    DownloadError::Request(error),
                ));
            }
        };
        let next_written = written.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = request.expected.size
            && next_written > expected_size
        {
            facts.push(size_mismatch_fact(&target, expected_size, next_written));
            drop(output);
            discard_download_temp(&tmp_path, &target, &mut facts).await;
            return Err(execution_download_error(
                ExecutionDownloadFactKind::SizeMismatch,
                facts,
                download_size_mismatch(request.destination, expected_size, next_written),
            ));
        }
        hasher.update(&chunk);
        if let Err(error) = output.write_all(&chunk).await {
            let kind = io_execution_fact_kind(error.kind());
            facts.push(execution_download_fact(
                kind,
                &target,
                no_download_fact_fields(),
            ));
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                &target,
                no_download_fact_fields(),
            ));
            drop(output);
            discard_download_temp(&tmp_path, &target, &mut facts).await;
            return Err(execution_download_error(
                ExecutionDownloadFactKind::TempWriteFailed,
                facts,
                DownloadError::FileOperation(error),
            ));
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
        discard_download_temp(&tmp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::Interrupted,
            facts,
            DownloadError::Integrity(format!(
                "{} download ended before the declared content length",
                bounded_download_file_label(request.destination)
            )),
        ));
    }
    if let Err(error) = output.flush().await {
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            &target,
            no_download_fact_fields(),
        ));
        drop(output);
        discard_download_temp(&tmp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::TempWriteFailed,
            facts,
            DownloadError::FileOperation(error),
        ));
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
        discard_download_temp(&tmp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            error_kind,
            facts,
            DownloadError::Integrity(error.to_string()),
        ));
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
        let kind = io_execution_fact_kind(error.kind());
        facts.push(execution_download_fact(
            kind,
            &target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::PromoteFailed,
            &target,
            no_download_fact_fields(),
        ));
        discard_download_temp(&tmp_path, &target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::PromoteFailed,
            facts,
            DownloadError::FileOperation(error),
        ));
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
    match async_fs::symlink_metadata(temp_path).await {
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
    match async_fs::symlink_metadata(destination).await {
        Ok(metadata) if metadata.is_file() || metadata.file_type().is_symlink() => {
            let backup_path = promotion_backup_path(destination);
            async_fs::rename(destination, &backup_path).await?;
            match async_fs::rename(temp_path, destination).await {
                Ok(()) => {
                    let _ = async_fs::remove_file(&backup_path).await;
                    Ok(())
                }
                Err(error) => {
                    let restore_result = async_fs::rename(&backup_path, destination).await;
                    restore_result?;
                    Err(error)
                }
            }
        }
        Ok(_) | Err(_) => async_fs::rename(temp_path, destination).await,
    }
}

pub(super) async fn remove_stale_download_temp(temp_path: &Path) -> Result<(), DownloadError> {
    let metadata = match async_fs::symlink_metadata(temp_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(DownloadError::FileOperation(error)),
    };
    let file_type = metadata.file_type();
    let result = if metadata.is_dir() && !file_type.is_symlink() {
        async_fs::remove_dir_all(temp_path).await
    } else {
        async_fs::remove_file(temp_path).await
    };

    result.map_err(DownloadError::FileOperation)
}

fn promotion_backup_path(destination: &Path) -> std::path::PathBuf {
    let mut extension = destination
        .extension()
        .map(|extension| extension.to_os_string())
        .unwrap_or_default();
    if !extension.is_empty() {
        extension.push(".");
    }
    extension.push(format!("croopor-backup-{}", std::process::id()));
    destination.with_extension(extension)
}
