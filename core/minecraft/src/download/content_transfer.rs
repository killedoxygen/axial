use super::facts::{
    execution_download_error, execution_download_fact, no_download_fact_fields, size_mismatch_fact,
};
use super::model::{
    DownloadError, ExecutionDownloadError, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExecutionDownloadReport, VerifiedContentIntegrity,
};
use super::path_safety::{
    bounded_download_file_label, filesystem_path, safe_download_target_label,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use sha2::Sha512;
use std::io;
use std::path::Path;
use std::time::Duration;
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

pub const MAX_VERIFIED_CONTENT_STAGING_BYTES: u64 = 1 << 30;

const CONTENT_DOWNLOAD_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];

/// Stream one provider artifact into an absent, caller-owned staging file.
///
/// This primitive verifies content bytes and leaves publication to the content
/// transaction that owns `staging_destination`. It never replaces or promotes
/// an arbitrary launcher-managed destination.
pub async fn download_verified_content_to_staging(
    client: &reqwest::Client,
    url: &str,
    staging_destination: &Path,
    expected: &VerifiedContentIntegrity,
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let retry_delays = CONTENT_DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis);
    download_verified_content_to_staging_with_retry_delays(
        client,
        url,
        staging_destination,
        expected,
        &retry_delays,
    )
    .await
}

async fn download_verified_content_to_staging_with_retry_delays(
    client: &reqwest::Client,
    url: &str,
    staging_destination: &Path,
    expected: &VerifiedContentIntegrity,
    retry_delays: &[Duration],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let target = safe_download_target_label(staging_destination);
    validate_content_integrity(expected, staging_destination, &target)?;
    validate_staging_destination(staging_destination, &target).await?;

    let mut prior_facts = Vec::new();
    let mut next_delay = 0_usize;
    loop {
        match download_verified_content_attempt(client, url, staging_destination, expected, &target)
            .await
        {
            Ok(mut report) => {
                prior_facts.append(&mut report.facts);
                report.facts = prior_facts;
                return Ok(report);
            }
            Err(mut error)
                if next_delay < retry_delays.len()
                    && execution_content_download_error_is_retryable(&error) =>
            {
                prior_facts.append(&mut error.facts);
                let delay = retry_delays[next_delay];
                next_delay += 1;
                tokio::time::sleep(delay).await;
            }
            Err(mut error) => {
                prior_facts.append(&mut error.facts);
                error.facts = prior_facts;
                return Err(error);
            }
        }
    }
}

fn validate_content_integrity(
    expected: &VerifiedContentIntegrity,
    staging_destination: &Path,
    target: &str,
) -> Result<(), ExecutionDownloadError> {
    let mut facts = Vec::new();
    if expected.sha1.is_none() && expected.sha512.is_none() {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataMissing,
            target,
            vec![("field", "checksum")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataMissing,
            facts,
            content_integrity_error(staging_destination, "is missing a checksum"),
        ));
    }
    if expected
        .sha1
        .as_deref()
        .is_some_and(|digest| !valid_hex_digest(digest, 40))
    {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            target,
            vec![("field", "sha1")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataInvalid,
            facts,
            content_integrity_error(staging_destination, "has an invalid sha1 checksum"),
        ));
    }
    if expected
        .sha512
        .as_deref()
        .is_some_and(|digest| !valid_hex_digest(digest, 128))
    {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            target,
            vec![("field", "sha512")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataInvalid,
            facts,
            content_integrity_error(staging_destination, "has an invalid sha512 checksum"),
        ));
    }
    if expected
        .size
        .is_some_and(|size| size == 0 || size > MAX_VERIFIED_CONTENT_STAGING_BYTES)
    {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            target,
            vec![
                ("field", "size".to_string()),
                ("max_bytes", MAX_VERIFIED_CONTENT_STAGING_BYTES.to_string()),
            ],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::MetadataInvalid,
            facts,
            content_integrity_error(staging_destination, "has an invalid size bound"),
        ));
    }
    Ok(())
}

async fn validate_staging_destination(
    staging_destination: &Path,
    target: &str,
) -> Result<(), ExecutionDownloadError> {
    let Some(parent) = staging_destination.parent() else {
        return Err(staging_io_error(
            target,
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "content staging destination has no parent",
            ),
        ));
    };
    match async_fs::symlink_metadata(filesystem_path(parent).as_ref()).await {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {}
        Ok(_) => {
            return Err(staging_io_error(
                target,
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "content staging parent is not a directory",
                ),
            ));
        }
        Err(error) => return Err(staging_io_error(target, error)),
    }
    match async_fs::symlink_metadata(filesystem_path(staging_destination).as_ref()).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(staging_io_error(target, error)),
        Ok(_) => Err(staging_io_error(
            target,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "content staging destination is already occupied",
            ),
        )),
    }
}

async fn download_verified_content_attempt(
    client: &reqwest::Client,
    url: &str,
    staging_destination: &Path,
    expected: &VerifiedContentIntegrity,
    target: &str,
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let mut facts = Vec::new();
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(error) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::NetworkFailure,
                target,
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
        let status = response.status().as_u16();
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::ProviderFailure,
            target,
            vec![("status", status.to_string())],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::ProviderFailure,
            facts,
            DownloadError::Request(error),
        ));
    }

    let byte_limit = expected.size.unwrap_or(MAX_VERIFIED_CONTENT_STAGING_BYTES);
    let declared_content_length = response.content_length();
    if let Some(content_length) = declared_content_length
        && content_length > byte_limit
    {
        facts.push(size_mismatch_fact(target, byte_limit, content_length));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::SizeMismatch,
            facts,
            download_size_mismatch(staging_destination, byte_limit, content_length),
        ));
    }

    let mut output = match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(filesystem_path(staging_destination).as_ref())
        .await
    {
        Ok(output) => output,
        Err(error) => return Err(staging_io_error(target, error)),
    };
    let mut sha1 = expected.sha1.is_some().then(Sha1::new);
    let mut sha512 = expected.sha512.is_some().then(Sha512::new);
    let mut written = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::Interrupted,
                    target,
                    no_download_fact_fields(),
                ));
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    target,
                    no_download_fact_fields(),
                ));
                drop(output);
                discard_failed_staging_file(staging_destination, target, &mut facts).await;
                return Err(execution_download_error(
                    ExecutionDownloadFactKind::Interrupted,
                    facts,
                    DownloadError::Request(error),
                ));
            }
        };
        let next_written = written.saturating_add(chunk.len() as u64);
        if next_written > byte_limit {
            facts.push(size_mismatch_fact(target, byte_limit, next_written));
            drop(output);
            discard_failed_staging_file(staging_destination, target, &mut facts).await;
            return Err(execution_download_error(
                ExecutionDownloadFactKind::SizeMismatch,
                facts,
                download_size_mismatch(staging_destination, byte_limit, next_written),
            ));
        }
        if let Some(hasher) = sha1.as_mut() {
            hasher.update(&chunk);
        }
        if let Some(hasher) = sha512.as_mut() {
            hasher.update(&chunk);
        }
        if let Err(error) = output.write_all(&chunk).await {
            drop(output);
            facts.push(execution_download_fact(
                execution_io_fact_kind(error.kind()),
                target,
                no_download_fact_fields(),
            ));
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                no_download_fact_fields(),
            ));
            discard_failed_staging_file(staging_destination, target, &mut facts).await;
            return Err(execution_download_error(
                execution_io_fact_kind(error.kind()),
                facts,
                DownloadError::FileOperation(error),
            ));
        }
        written = next_written;
    }

    if declared_content_length.is_some_and(|content_length| content_length != written)
        || expected
            .size
            .is_some_and(|expected_size| expected_size > written)
    {
        let expected_size = declared_content_length
            .or(expected.size)
            .unwrap_or(byte_limit);
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::Interrupted,
            target,
            no_download_fact_fields(),
        ));
        facts.push(size_mismatch_fact(target, expected_size, written));
        drop(output);
        discard_failed_staging_file(staging_destination, target, &mut facts).await;
        return Err(execution_download_error(
            ExecutionDownloadFactKind::Interrupted,
            facts,
            DownloadError::Integrity(format!(
                "{} download ended before its declared size",
                bounded_download_file_label(staging_destination)
            )),
        ));
    }
    if let Err(error) = output.flush().await {
        drop(output);
        facts.push(execution_download_fact(
            execution_io_fact_kind(error.kind()),
            target,
            no_download_fact_fields(),
        ));
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            target,
            no_download_fact_fields(),
        ));
        discard_failed_staging_file(staging_destination, target, &mut facts).await;
        return Err(execution_download_error(
            execution_io_fact_kind(error.kind()),
            facts,
            DownloadError::FileOperation(error),
        ));
    }
    drop(output);

    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let actual = format!(
            "{:x}",
            sha1.take()
                .expect("sha1 hasher exists when sha1 is expected")
                .finalize()
        );
        if !actual.eq_ignore_ascii_case(expected_sha1) {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha1")],
            ));
            discard_failed_staging_file(staging_destination, target, &mut facts).await;
            return Err(execution_download_error(
                ExecutionDownloadFactKind::ChecksumMismatch,
                facts,
                content_integrity_error(staging_destination, "failed sha1 verification"),
            ));
        }
    }
    if let Some(expected_sha512) = expected.sha512.as_deref() {
        let actual = format!(
            "{:x}",
            sha512
                .take()
                .expect("sha512 hasher exists when sha512 is expected")
                .finalize()
        );
        if !actual.eq_ignore_ascii_case(expected_sha512) {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha512")],
            ));
            discard_failed_staging_file(staging_destination, target, &mut facts).await;
            return Err(execution_download_error(
                ExecutionDownloadFactKind::ChecksumMismatch,
                facts,
                content_integrity_error(staging_destination, "failed sha512 verification"),
            ));
        }
    }

    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        target,
        vec![("bytes", written.to_string())],
    ));
    Ok(ExecutionDownloadReport {
        target: target.to_string(),
        bytes_written: written,
        facts,
    })
}

async fn discard_failed_staging_file(
    staging_destination: &Path,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
) {
    match async_fs::remove_file(filesystem_path(staging_destination).as_ref()).await {
        Ok(()) => facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempDiscarded,
            target,
            no_download_fact_fields(),
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(_) => facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            target,
            no_download_fact_fields(),
        )),
    }
}

fn execution_content_download_error_is_retryable(error: &ExecutionDownloadError) -> bool {
    match error.kind {
        ExecutionDownloadFactKind::NetworkFailure | ExecutionDownloadFactKind::Interrupted => true,
        ExecutionDownloadFactKind::ProviderFailure => provider_failure_status(error)
            .is_some_and(|status| status == 408 || status == 429 || (500..=599).contains(&status)),
        ExecutionDownloadFactKind::ChecksumMismatch
        | ExecutionDownloadFactKind::MetadataInvalid
        | ExecutionDownloadFactKind::MetadataMissing
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
    error
        .facts
        .iter()
        .filter(|fact| fact.kind == ExecutionDownloadFactKind::ProviderFailure)
        .flat_map(|fact| fact.fields.iter())
        .find_map(|(key, value)| (key == "status").then(|| value.parse().ok()).flatten())
}

fn valid_hex_digest(digest: &str, expected_len: usize) -> bool {
    digest.len() == expected_len && digest.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn execution_io_fact_kind(kind: io::ErrorKind) -> ExecutionDownloadFactKind {
    match kind {
        io::ErrorKind::PermissionDenied => ExecutionDownloadFactKind::PermissionFailure,
        _ => ExecutionDownloadFactKind::TempWriteFailed,
    }
}

fn staging_io_error(target: &str, error: io::Error) -> ExecutionDownloadError {
    let kind = execution_io_fact_kind(error.kind());
    execution_download_error(
        kind,
        vec![execution_download_fact(
            kind,
            target,
            no_download_fact_fields(),
        )],
        DownloadError::FileOperation(error),
    )
}

fn content_integrity_error(destination: &Path, detail: &str) -> DownloadError {
    DownloadError::Integrity(format!(
        "{} integrity metadata {detail}",
        bounded_download_file_label(destination)
    ))
}

fn download_size_mismatch(destination: &Path, expected: u64, actual: u64) -> DownloadError {
    DownloadError::Integrity(format!(
        "{} size mismatch: expected at most {expected}, got {actual}",
        bounded_download_file_label(destination)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::TempDir;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[tokio::test]
    async fn stages_verified_sha1_and_sha512_without_publication_facts() {
        let body = b"verified content".to_vec();
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(vec![(200, body.clone())], Arc::clone(&attempts)).await;
        let root = TempDir::new().expect("staging root");
        let destination = root.path().join("mods").join("content.jar");
        async_fs::create_dir_all(destination.parent().expect("parent"))
            .await
            .expect("staging parent");
        let expected = integrity(&body);

        let report = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &expected,
            &[],
        )
        .await
        .expect("verified stage");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(report.bytes_written, body.len() as u64);
        assert_eq!(async_fs::read(&destination).await.expect("staged"), body);
        assert!(
            report
                .facts
                .iter()
                .any(|fact| fact.kind == ExecutionDownloadFactKind::WrittenToTemp)
        );
        assert!(
            report
                .facts
                .iter()
                .all(|fact| fact.kind != ExecutionDownloadFactKind::Promoted)
        );
    }

    #[tokio::test]
    async fn retries_transient_provider_failure_and_retains_attempt_facts() {
        let body = b"eventual content".to_vec();
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(
            vec![(503, Vec::new()), (200, body.clone())],
            Arc::clone(&attempts),
        )
        .await;
        let root = TempDir::new().expect("staging root");
        let destination = root.path().join("content.jar");

        let report = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &integrity(&body),
            &[Duration::ZERO],
        )
        .await
        .expect("retry succeeds");

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(report.facts.iter().any(|fact| {
            fact.kind == ExecutionDownloadFactKind::ProviderFailure
                && fact.fields == [("status".to_string(), "503".to_string())]
        }));
    }

    #[tokio::test]
    async fn checksum_mismatch_is_not_retried_and_discards_partial_stage() {
        let body = b"EXPECTED CONTENT".to_vec();
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(
            vec![(200, body.clone()), (200, body)],
            Arc::clone(&attempts),
        )
        .await;
        let root = TempDir::new().expect("staging root");
        let destination = root.path().join("private").join("content.jar");
        async_fs::create_dir_all(destination.parent().expect("parent"))
            .await
            .expect("staging parent");
        let expected = integrity(b"expected content");

        let error = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &expected,
            &[Duration::ZERO],
        )
        .await
        .expect_err("checksum mismatch");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(error.kind, ExecutionDownloadFactKind::ChecksumMismatch);
        assert!(!destination.exists());
        assert!(error.facts.iter().all(|fact| {
            !fact.target.contains('/')
                && !fact.target.contains('\\')
                && !fact.fields.iter().any(|(_, value)| value.contains('/'))
        }));
    }

    #[tokio::test]
    async fn permanent_provider_failure_is_not_retried() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(
            vec![(404, Vec::new()), (200, b"unused".to_vec())],
            Arc::clone(&attempts),
        )
        .await;
        let root = TempDir::new().expect("staging root");
        let destination = root.path().join("content.jar");

        let error = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &integrity(b"unused"),
            &[Duration::ZERO],
        )
        .await
        .expect_err("404 is permanent");

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert_eq!(error.kind, ExecutionDownloadFactKind::ProviderFailure);
        assert!(!destination.exists());
    }

    #[tokio::test]
    async fn missing_checksum_and_occupied_stage_fail_before_network() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(vec![(200, b"unused".to_vec())], Arc::clone(&attempts)).await;
        let root = TempDir::new().expect("staging root");
        let destination = root.path().join("content.jar");

        let missing = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &VerifiedContentIntegrity::default(),
            &[],
        )
        .await
        .expect_err("checksum required");
        assert_eq!(missing.kind, ExecutionDownloadFactKind::MetadataMissing);

        async_fs::write(&destination, b"user bytes")
            .await
            .expect("occupied stage");
        let occupied = download_verified_content_to_staging_with_retry_delays(
            &reqwest::Client::new(),
            &url,
            &destination,
            &integrity(b"unused"),
            &[],
        )
        .await
        .expect_err("occupied staging destination");
        assert_eq!(occupied.kind, ExecutionDownloadFactKind::TempWriteFailed);
        assert_eq!(
            async_fs::read(&destination).await.expect("preserved"),
            b"user bytes"
        );
        assert_eq!(attempts.load(Ordering::SeqCst), 0);
    }

    fn integrity(body: &[u8]) -> VerifiedContentIntegrity {
        VerifiedContentIntegrity {
            size: Some(body.len() as u64),
            sha1: Some(format!("{:x}", Sha1::digest(body))),
            sha512: Some(format!("{:x}", Sha512::digest(body))),
        }
    }

    async fn serve_responses(responses: Vec<(u16, Vec<u8>)>, attempts: Arc<AtomicUsize>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
        let address = listener.local_addr().expect("address");
        tokio::spawn(async move {
            for (status, body) in responses {
                let (mut stream, _) = listener.accept().await.expect("accept");
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await;
                attempts.fetch_add(1, Ordering::SeqCst);
                let reason = if status == 200 {
                    "OK"
                } else {
                    "Service Unavailable"
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream
                    .write_all(response.as_bytes())
                    .await
                    .expect("headers");
                stream.write_all(&body).await.expect("body");
            }
        });
        format!("http://{address}/artifact")
    }
}
