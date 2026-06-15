//! Execution-owned download capabilities.
//!
//! These helpers own primitive transport, byte bounds, verification, temporary
//! file cleanup, and final promotion. They emit structured facts and leave
//! retry/fallback policy to Guardian/Application callers.

use super::file::{
    PromoteTempFileRequest, io_error_fact, promote_temp_file, validate_managed_ownership,
};
use super::{ExecutionFact, ExecutionFactKind};
use crate::observability::{EvidenceField, EvidenceSensitivity};
use crate::state::contracts::{OperationId, TargetDescriptor};
use futures_util::StreamExt;
use reqwest::Client;
use sha1::Sha1;
use sha2::{Digest, Sha256};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::fs as async_fs;
use tokio::io::AsyncWriteExt;

#[derive(Clone, Debug)]
pub struct DownloadToTempRequest<'a> {
    pub operation_id: Option<OperationId>,
    pub target: TargetDescriptor,
    pub url: &'a str,
    pub destination: &'a Path,
    pub max_bytes: Option<u64>,
    pub expected_size: Option<u64>,
    pub expected_checksum: Option<DownloadChecksum<'a>>,
}

impl<'a> DownloadToTempRequest<'a> {
    pub fn new(target: TargetDescriptor, destination: &'a Path, url: &'a str) -> Self {
        Self {
            operation_id: None,
            target,
            url,
            destination,
            max_bytes: None,
            expected_size: None,
            expected_checksum: None,
        }
    }

    pub fn with_max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = Some(max_bytes);
        self
    }

    pub fn with_expected_size(mut self, expected_size: u64) -> Self {
        self.expected_size = Some(expected_size);
        self
    }

    pub fn with_expected_checksum(mut self, expected_checksum: DownloadChecksum<'a>) -> Self {
        self.expected_checksum = Some(expected_checksum);
        self
    }

    pub fn with_expected_sha1(mut self, expected_sha1: &'a str) -> Self {
        self.expected_checksum = Some(DownloadChecksum::sha1(expected_sha1));
        self
    }

    pub fn with_expected_sha256(mut self, expected_sha256: &'a str) -> Self {
        self.expected_checksum = Some(DownloadChecksum::sha256(expected_sha256));
        self
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DownloadChecksum<'a> {
    pub algorithm: DownloadChecksumAlgorithm,
    pub value: &'a str,
}

impl<'a> DownloadChecksum<'a> {
    pub fn new(algorithm: DownloadChecksumAlgorithm, value: &'a str) -> Self {
        Self { algorithm, value }
    }

    pub fn sha1(value: &'a str) -> Self {
        Self::new(DownloadChecksumAlgorithm::Sha1, value)
    }

    pub fn sha256(value: &'a str) -> Self {
        Self::new(DownloadChecksumAlgorithm::Sha256, value)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadChecksumAlgorithm {
    Sha1,
    Sha256,
}

impl DownloadChecksumAlgorithm {
    pub fn parse(value: &str) -> Option<Self> {
        match value.trim().to_ascii_lowercase().as_str() {
            "sha1" | "sha-1" => Some(Self::Sha1),
            "sha256" | "sha-256" => Some(Self::Sha256),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sha1 => "sha1",
            Self::Sha256 => "sha256",
        }
    }

    fn hex_len(self) -> usize {
        match self {
            Self::Sha1 => 40,
            Self::Sha256 => 64,
        }
    }
}

pub fn valid_download_checksum_metadata(checksum: DownloadChecksum<'_>) -> bool {
    normalize_checksum(checksum).is_some()
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DownloadCapabilityReport {
    pub target: TargetDescriptor,
    pub bytes_written: u64,
    pub facts: Vec<ExecutionFact>,
}

#[derive(Debug)]
pub struct DownloadCapabilityError {
    pub kind: DownloadCapabilityErrorKind,
    pub facts: Vec<ExecutionFact>,
}

impl DownloadCapabilityError {
    fn new(kind: DownloadCapabilityErrorKind, facts: Vec<ExecutionFact>) -> Self {
        Self { kind, facts }
    }
}

impl fmt::Display for DownloadCapabilityError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            DownloadCapabilityErrorKind::OwnershipRefused => {
                formatter.write_str("download capability refused target ownership")
            }
            DownloadCapabilityErrorKind::NetworkFailure => {
                formatter.write_str("download request failed")
            }
            DownloadCapabilityErrorKind::ProviderFailure => {
                formatter.write_str("download provider returned an unsuccessful status")
            }
            DownloadCapabilityErrorKind::CreateParentFailed => {
                formatter.write_str("download capability failed to create parent directory")
            }
            DownloadCapabilityErrorKind::TempWriteFailed => {
                formatter.write_str("download capability failed to write temporary file")
            }
            DownloadCapabilityErrorKind::SizeMismatch => {
                formatter.write_str("download size verification failed")
            }
            DownloadCapabilityErrorKind::ChecksumMismatch => {
                formatter.write_str("download checksum verification failed")
            }
            DownloadCapabilityErrorKind::ExpectedChecksumInvalid => {
                formatter.write_str("download checksum metadata is invalid")
            }
            DownloadCapabilityErrorKind::PromoteFailed => {
                formatter.write_str("download capability failed to promote temporary file")
            }
        }
    }
}

impl std::error::Error for DownloadCapabilityError {}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DownloadCapabilityErrorKind {
    OwnershipRefused,
    NetworkFailure,
    ProviderFailure,
    CreateParentFailed,
    TempWriteFailed,
    SizeMismatch,
    ChecksumMismatch,
    ExpectedChecksumInvalid,
    PromoteFailed,
}

pub async fn download_url_to_temp(
    request: DownloadToTempRequest<'_>,
    client: &Client,
) -> Result<DownloadCapabilityReport, DownloadCapabilityError> {
    let mut facts = Vec::new();
    if let Err(error) =
        validate_managed_ownership(&request.target, request.operation_id.as_ref(), &mut facts)
    {
        return Err(DownloadCapabilityError::new(
            DownloadCapabilityErrorKind::OwnershipRefused,
            error.facts,
        ));
    }

    let expected_checksum = if let Some(expected_checksum) = request.expected_checksum {
        match normalize_checksum(expected_checksum) {
            Some(expected_checksum) => Some(expected_checksum),
            None => {
                facts.push(download_fact(
                    ExecutionFactKind::ProviderDataInvalid,
                    request.operation_id.clone(),
                    &request.target,
                    vec![EvidenceField::new(
                        "algorithm",
                        expected_checksum.algorithm.as_str(),
                        EvidenceSensitivity::Public,
                    )],
                ));
                return Err(DownloadCapabilityError::new(
                    DownloadCapabilityErrorKind::ExpectedChecksumInvalid,
                    facts,
                ));
            }
        }
    } else {
        None
    };

    if let Some(parent) = request.destination.parent()
        && let Err(error) = async_fs::create_dir_all(parent).await
    {
        facts.push(io_error_fact(
            error.kind(),
            request.operation_id.clone(),
            &request.target,
        ));
        facts.push(download_fact(
            ExecutionFactKind::DownloadTempWriteFailed,
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        return Err(DownloadCapabilityError::new(
            DownloadCapabilityErrorKind::CreateParentFailed,
            facts,
        ));
    }

    let response = match client.get(request.url).send().await {
        Ok(response) => response,
        Err(_) => {
            facts.push(download_fact(
                ExecutionFactKind::DownloadNetworkFailure,
                request.operation_id.clone(),
                &request.target,
                Vec::new(),
            ));
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::NetworkFailure,
                facts,
            ));
        }
    };

    let status = response.status();
    if !status.is_success() {
        facts.push(download_fact(
            ExecutionFactKind::DownloadProviderFailure,
            request.operation_id.clone(),
            &request.target,
            vec![EvidenceField::new(
                "status",
                status.as_u16().to_string(),
                EvidenceSensitivity::Public,
            )],
        ));
        return Err(DownloadCapabilityError::new(
            DownloadCapabilityErrorKind::ProviderFailure,
            facts,
        ));
    }

    if let Some(content_length) = response.content_length() {
        if let Some(max_bytes) = request.max_bytes
            && content_length > max_bytes
        {
            facts.push(size_limit_fact(
                request.operation_id.clone(),
                &request.target,
                max_bytes,
                content_length,
            ));
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::SizeMismatch,
                facts,
            ));
        }
        if let Some(expected_size) = request.expected_size
            && content_length != expected_size
        {
            facts.push(size_mismatch_fact(
                request.operation_id.clone(),
                &request.target,
                expected_size,
                content_length,
            ));
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::SizeMismatch,
                facts,
            ));
        }
    }

    let temp_path = download_temp_path_for(request.destination);
    let mut output = match async_fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp_path)
        .await
    {
        Ok(output) => output,
        Err(error) => {
            facts.push(io_error_fact(
                error.kind(),
                request.operation_id.clone(),
                &request.target,
            ));
            facts.push(download_fact(
                ExecutionFactKind::DownloadTempWriteFailed,
                request.operation_id.clone(),
                &request.target,
                Vec::new(),
            ));
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::TempWriteFailed,
                facts,
            ));
        }
    };

    let mut bytes_written = 0_u64;
    let mut hasher = expected_checksum
        .as_ref()
        .map(|checksum| ActiveDownloadHasher::new(checksum.algorithm));
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(_) => {
                facts.push(download_fact(
                    ExecutionFactKind::DownloadInterrupted,
                    request.operation_id.clone(),
                    &request.target,
                    Vec::new(),
                ));
                facts.push(download_fact(
                    ExecutionFactKind::DownloadNetworkFailure,
                    request.operation_id.clone(),
                    &request.target,
                    Vec::new(),
                ));
                drop(output);
                discard_temp_file(
                    &temp_path,
                    &mut facts,
                    request.operation_id.as_ref(),
                    &request.target,
                )
                .await;
                return Err(DownloadCapabilityError::new(
                    DownloadCapabilityErrorKind::NetworkFailure,
                    facts,
                ));
            }
        };

        let next_size = bytes_written.saturating_add(chunk.len() as u64);
        if let Some(max_bytes) = request.max_bytes
            && next_size > max_bytes
        {
            facts.push(size_limit_fact(
                request.operation_id.clone(),
                &request.target,
                max_bytes,
                next_size,
            ));
            drop(output);
            discard_temp_file(
                &temp_path,
                &mut facts,
                request.operation_id.as_ref(),
                &request.target,
            )
            .await;
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::SizeMismatch,
                facts,
            ));
        }

        if let Err(error) = output.write_all(&chunk).await {
            facts.push(io_error_fact(
                error.kind(),
                request.operation_id.clone(),
                &request.target,
            ));
            facts.push(download_fact(
                ExecutionFactKind::DownloadTempWriteFailed,
                request.operation_id.clone(),
                &request.target,
                Vec::new(),
            ));
            drop(output);
            discard_temp_file(
                &temp_path,
                &mut facts,
                request.operation_id.as_ref(),
                &request.target,
            )
            .await;
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::TempWriteFailed,
                facts,
            ));
        }
        if let Some(hasher) = hasher.as_mut() {
            hasher.update(&chunk);
        }
        bytes_written = next_size;
    }

    if let Err(error) = output.flush().await {
        facts.push(io_error_fact(
            error.kind(),
            request.operation_id.clone(),
            &request.target,
        ));
        facts.push(download_fact(
            ExecutionFactKind::DownloadTempWriteFailed,
            request.operation_id.clone(),
            &request.target,
            Vec::new(),
        ));
        drop(output);
        discard_temp_file(
            &temp_path,
            &mut facts,
            request.operation_id.as_ref(),
            &request.target,
        )
        .await;
        return Err(DownloadCapabilityError::new(
            DownloadCapabilityErrorKind::TempWriteFailed,
            facts,
        ));
    }
    drop(output);
    facts.push(download_fact(
        ExecutionFactKind::DownloadWrittenToTemp,
        request.operation_id.clone(),
        &request.target,
        vec![EvidenceField::new(
            "bytes",
            bytes_written.to_string(),
            EvidenceSensitivity::Public,
        )],
    ));

    if let Some(expected_size) = request.expected_size
        && bytes_written != expected_size
    {
        facts.push(size_mismatch_fact(
            request.operation_id.clone(),
            &request.target,
            expected_size,
            bytes_written,
        ));
        discard_temp_file(
            &temp_path,
            &mut facts,
            request.operation_id.as_ref(),
            &request.target,
        )
        .await;
        return Err(DownloadCapabilityError::new(
            DownloadCapabilityErrorKind::SizeMismatch,
            facts,
        ));
    }

    if let Some(expected_checksum) = expected_checksum {
        let actual_checksum = hasher
            .expect("checksum hasher exists when expected checksum exists")
            .finalize_hex();
        if actual_checksum != expected_checksum.value {
            facts.push(download_fact(
                ExecutionFactKind::DownloadChecksumMismatch,
                request.operation_id.clone(),
                &request.target,
                vec![EvidenceField::new(
                    "algorithm",
                    expected_checksum.algorithm.as_str(),
                    EvidenceSensitivity::Public,
                )],
            ));
            discard_temp_file(
                &temp_path,
                &mut facts,
                request.operation_id.as_ref(),
                &request.target,
            )
            .await;
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::ChecksumMismatch,
                facts,
            ));
        }
    }

    let promote_report = match promote_temp_file(PromoteTempFileRequest {
        operation_id: request.operation_id.clone(),
        target: request.target.clone(),
        temp_path: &temp_path,
        destination: request.destination,
    }) {
        Ok(report) => report,
        Err(error) => {
            facts.extend(error.facts);
            return Err(DownloadCapabilityError::new(
                DownloadCapabilityErrorKind::PromoteFailed,
                facts,
            ));
        }
    };
    facts.extend(promote_report.facts);
    facts.push(download_fact(
        ExecutionFactKind::DownloadPromoted,
        request.operation_id.clone(),
        &request.target,
        Vec::new(),
    ));

    Ok(DownloadCapabilityReport {
        target: request.target,
        bytes_written,
        facts,
    })
}

pub fn download_fact(
    kind: ExecutionFactKind,
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    extra_fields: Vec<EvidenceField>,
) -> ExecutionFact {
    let mut fields = vec![EvidenceField::new(
        "target",
        target.id.clone(),
        EvidenceSensitivity::Public,
    )];
    fields.extend(extra_fields);
    ExecutionFact {
        operation_id,
        kind,
        target: Some(target.clone()),
        fields,
    }
}

fn size_limit_fact(
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    limit: u64,
    actual: u64,
) -> ExecutionFact {
    download_fact(
        ExecutionFactKind::DownloadSizeMismatch,
        operation_id,
        target,
        vec![
            EvidenceField::new(
                "limit_bytes",
                limit.to_string(),
                EvidenceSensitivity::Public,
            ),
            EvidenceField::new(
                "actual_bytes",
                actual.to_string(),
                EvidenceSensitivity::Public,
            ),
        ],
    )
}

fn size_mismatch_fact(
    operation_id: Option<OperationId>,
    target: &TargetDescriptor,
    expected: u64,
    actual: u64,
) -> ExecutionFact {
    download_fact(
        ExecutionFactKind::DownloadSizeMismatch,
        operation_id,
        target,
        vec![
            EvidenceField::new(
                "expected_bytes",
                expected.to_string(),
                EvidenceSensitivity::Public,
            ),
            EvidenceField::new(
                "actual_bytes",
                actual.to_string(),
                EvidenceSensitivity::Public,
            ),
        ],
    )
}

async fn discard_temp_file(
    temp_path: &Path,
    facts: &mut Vec<ExecutionFact>,
    operation_id: Option<&OperationId>,
    target: &TargetDescriptor,
) {
    match async_fs::remove_file(temp_path).await {
        Ok(()) => facts.push(download_fact(
            ExecutionFactKind::DownloadTempDiscarded,
            operation_id.cloned(),
            target,
            Vec::new(),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => facts.push(io_error_fact(error.kind(), operation_id.cloned(), target)),
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct NormalizedDownloadChecksum {
    algorithm: DownloadChecksumAlgorithm,
    value: String,
}

fn normalize_checksum(checksum: DownloadChecksum<'_>) -> Option<NormalizedDownloadChecksum> {
    let value = checksum.value.trim();
    if value.len() == checksum.algorithm.hex_len()
        && value.bytes().all(|value| value.is_ascii_hexdigit())
    {
        Some(NormalizedDownloadChecksum {
            algorithm: checksum.algorithm,
            value: value.to_ascii_lowercase(),
        })
    } else {
        None
    }
}

enum ActiveDownloadHasher {
    Sha1(Sha1),
    Sha256(Sha256),
}

impl ActiveDownloadHasher {
    fn new(algorithm: DownloadChecksumAlgorithm) -> Self {
        match algorithm {
            DownloadChecksumAlgorithm::Sha1 => Self::Sha1(Sha1::new()),
            DownloadChecksumAlgorithm::Sha256 => Self::Sha256(Sha256::new()),
        }
    }

    fn update(&mut self, chunk: &[u8]) {
        match self {
            Self::Sha1(hasher) => hasher.update(chunk),
            Self::Sha256(hasher) => hasher.update(chunk),
        }
    }

    fn finalize_hex(self) -> String {
        match self {
            Self::Sha1(hasher) => format!("{:x}", hasher.finalize()),
            Self::Sha256(hasher) => format!("{:x}", hasher.finalize()),
        }
    }
}

fn download_temp_path_for(destination: &Path) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    let suffix = match destination.extension().and_then(|value| value.to_str()) {
        Some(extension) if !extension.is_empty() => {
            format!("{extension}.download.tmp-{}-{nanos:x}", std::process::id())
        }
        _ => format!("download.tmp-{}-{nanos:x}", std::process::id()),
    };
    destination.with_extension(suffix)
}

#[cfg(test)]
mod tests {
    use super::{
        DownloadCapabilityErrorKind, DownloadChecksum, DownloadChecksumAlgorithm,
        DownloadToTempRequest, download_url_to_temp, normalize_checksum,
        valid_download_checksum_metadata,
    };
    use crate::execution::ExecutionFactKind;
    use crate::state::contracts::{
        OwnershipClass, StabilizationSystem, TargetDescriptor, TargetKind,
    };
    use reqwest::Client;
    use sha1::Sha1;
    use sha2::{Digest, Sha256};
    use std::fs;
    use std::path::{Path, PathBuf};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn download_promotes_successful_response() {
        let root = test_root("success");
        let destination = root.join("track.mp3");
        let (url, server) =
            spawn_download_server("200 OK", b"fresh music".to_vec(), Some(11)).await;
        let expected_sha256 = sha256_hex(b"fresh music");

        let report = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("music_cache"), &destination, &url)
                .with_max_bytes(1024)
                .with_expected_size(11)
                .with_expected_sha256(&expected_sha256),
            &Client::new(),
        )
        .await
        .expect("download should promote");
        server.await.expect("server task");

        assert_eq!(
            fs::read(&destination).expect("read promoted destination"),
            b"fresh music"
        );
        assert_eq!(report.bytes_written, 11);
        assert!(has_fact(
            &report.facts,
            ExecutionFactKind::DownloadWrittenToTemp
        ));
        assert!(has_fact(&report.facts, ExecutionFactKind::FilePromoted));
        assert!(has_fact(&report.facts, ExecutionFactKind::DownloadPromoted));
        assert_no_temp_files(&root, &destination);

        cleanup(&root);
    }

    #[tokio::test]
    async fn download_verifies_sha1_checksum_before_promotion() {
        let root = test_root("sha1-success");
        let destination = root.join("client.jar");
        let (url, server) =
            spawn_download_server("200 OK", b"minecraft artifact".to_vec(), None).await;
        let expected_sha1 = sha1_hex(b"minecraft artifact");

        let report = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("client_jar"), &destination, &url)
                .with_max_bytes(1024)
                .with_expected_sha1(&expected_sha1),
            &Client::new(),
        )
        .await
        .expect("download should promote");
        server.await.expect("server task");

        assert_eq!(
            fs::read(&destination).expect("read promoted destination"),
            b"minecraft artifact"
        );
        assert_eq!(report.bytes_written, 18);
        assert!(has_fact(&report.facts, ExecutionFactKind::DownloadPromoted));
        assert_no_temp_files(&root, &destination);

        cleanup(&root);
    }

    #[tokio::test]
    async fn checksum_mismatch_does_not_promote_target() {
        let root = test_root("checksum-mismatch");
        let destination = root.join("track.mp3");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing music").expect("write existing destination");
        let (url, server) = spawn_download_server("200 OK", b"wrong music".to_vec(), None).await;
        let expected_sha256 = sha256_hex(b"right music");

        let error = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("music_cache"), &destination, &url)
                .with_max_bytes(1024)
                .with_expected_sha256(&expected_sha256),
            &Client::new(),
        )
        .await
        .expect_err("checksum mismatch should fail");
        server.await.expect("server task");

        assert_eq!(error.kind, DownloadCapabilityErrorKind::ChecksumMismatch);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"existing music"
        );
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadChecksumMismatch
        ));
        assert!(fact_has_field(
            &error.facts,
            ExecutionFactKind::DownloadChecksumMismatch,
            "algorithm",
            "sha256",
        ));
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadTempDiscarded
        ));
        assert_no_temp_files(&root, &destination);
        assert_no_raw_path_or_url(&error.facts, &root, &url);

        cleanup(&root);
    }

    #[tokio::test]
    async fn sha1_checksum_mismatch_reports_algorithm_and_preserves_target() {
        let root = test_root("sha1-checksum-mismatch");
        let destination = root.join("client.jar");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing jar").expect("write existing destination");
        let (url, server) = spawn_download_server("200 OK", b"wrong jar".to_vec(), None).await;
        let expected_sha1 = sha1_hex(b"right jar");

        let error = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("client_jar"), &destination, &url)
                .with_max_bytes(1024)
                .with_expected_sha1(&expected_sha1),
            &Client::new(),
        )
        .await
        .expect_err("checksum mismatch should fail");
        server.await.expect("server task");

        assert_eq!(error.kind, DownloadCapabilityErrorKind::ChecksumMismatch);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"existing jar"
        );
        assert!(fact_has_field(
            &error.facts,
            ExecutionFactKind::DownloadChecksumMismatch,
            "algorithm",
            "sha1",
        ));
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadTempDiscarded
        ));
        assert_no_temp_files(&root, &destination);

        cleanup(&root);
    }

    #[tokio::test]
    async fn size_mismatch_does_not_promote_target() {
        let root = test_root("size-mismatch");
        let destination = root.join("track.mp3");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing music").expect("write existing destination");
        let (url, server) = spawn_download_server("200 OK", b"short".to_vec(), None).await;

        let error = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("music_cache"), &destination, &url)
                .with_max_bytes(1024)
                .with_expected_size(32),
            &Client::new(),
        )
        .await
        .expect_err("size mismatch should fail");
        server.await.expect("server task");

        assert_eq!(error.kind, DownloadCapabilityErrorKind::SizeMismatch);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"existing music"
        );
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadSizeMismatch
        ));
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadTempDiscarded
        ));
        assert_no_temp_files(&root, &destination);

        cleanup(&root);
    }

    #[tokio::test]
    async fn interrupted_download_discards_temp_and_reports_facts() {
        let root = test_root("interrupted");
        let destination = root.join("track.mp3");
        fs::create_dir_all(&root).expect("create root");
        fs::write(&destination, b"existing music").expect("write existing destination");
        let (url, server) = spawn_download_server("200 OK", b"partial".to_vec(), Some(64)).await;

        let error = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("music_cache"), &destination, &url)
                .with_max_bytes(1024),
            &Client::new(),
        )
        .await
        .expect_err("interrupted stream should fail");
        server.await.expect("server task");

        assert_eq!(error.kind, DownloadCapabilityErrorKind::NetworkFailure);
        assert_eq!(
            fs::read(&destination).expect("read preserved destination"),
            b"existing music"
        );
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadInterrupted
        ));
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadNetworkFailure
        ));
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadTempDiscarded
        ));
        assert_no_temp_files(&root, &destination);
        assert_no_raw_path_or_url(&error.facts, &root, &url);

        cleanup(&root);
    }

    #[tokio::test]
    async fn provider_status_reports_provider_failure_without_temp_reuse() {
        let root = test_root("provider-status");
        let destination = root.join("track.mp3");
        let (url, server) = spawn_download_server(
            "503 Service Unavailable",
            b"provider details".to_vec(),
            Some(16),
        )
        .await;

        let error = download_url_to_temp(
            DownloadToTempRequest::new(launcher_target("music_cache"), &destination, &url)
                .with_max_bytes(1024),
            &Client::new(),
        )
        .await
        .expect_err("provider status should fail");
        server.await.expect("server task");

        assert_eq!(error.kind, DownloadCapabilityErrorKind::ProviderFailure);
        assert!(!destination.exists());
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::DownloadProviderFailure
        ));
        assert_no_temp_files(&root, &destination);
        assert_no_raw_path_or_url(&error.facts, &root, &url);

        cleanup(&root);
    }

    #[test]
    fn checksum_metadata_requires_matching_hex_algorithm() {
        assert!(normalize_checksum(DownloadChecksum::sha1(&sha1_hex(b"music"))).is_some());
        assert!(normalize_checksum(DownloadChecksum::sha256(&sha256_hex(b"music"))).is_some());
        assert!(normalize_checksum(DownloadChecksum::sha1(&sha256_hex(b"music"))).is_none());
        assert!(normalize_checksum(DownloadChecksum::sha256(&sha1_hex(b"music"))).is_none());
        assert!(normalize_checksum(DownloadChecksum::sha1("-Xmx8192M")).is_none());
        assert_eq!(
            DownloadChecksumAlgorithm::parse("sha-1"),
            Some(DownloadChecksumAlgorithm::Sha1)
        );
        assert_eq!(
            DownloadChecksumAlgorithm::parse("sha-256"),
            Some(DownloadChecksumAlgorithm::Sha256)
        );
        assert_eq!(DownloadChecksumAlgorithm::parse("sha512"), None);
        assert!(valid_download_checksum_metadata(DownloadChecksum::sha1(
            &sha1_hex(b"music")
        )));
    }

    #[tokio::test]
    async fn invalid_checksum_metadata_fails_before_io_or_network() {
        let root = test_root("invalid-checksum");
        let destination = root.join("client.jar");

        let error = download_url_to_temp(
            DownloadToTempRequest::new(
                launcher_target("client_jar"),
                &destination,
                "http://127.0.0.1:1/artifact.jar",
            )
            .with_expected_checksum(DownloadChecksum::new(
                DownloadChecksumAlgorithm::Sha1,
                "-Xmx8192M",
            )),
            &Client::new(),
        )
        .await
        .expect_err("invalid checksum metadata should fail");

        assert_eq!(
            error.kind,
            DownloadCapabilityErrorKind::ExpectedChecksumInvalid
        );
        assert!(!root.exists());
        assert!(has_fact(
            &error.facts,
            ExecutionFactKind::ProviderDataInvalid
        ));
        assert!(fact_has_field(
            &error.facts,
            ExecutionFactKind::ProviderDataInvalid,
            "algorithm",
            "sha1",
        ));
        assert_no_raw_path_or_url(&error.facts, &root, "http://127.0.0.1:1/artifact.jar");
    }

    async fn spawn_download_server(
        status: &'static str,
        body: Vec<u8>,
        content_length: Option<usize>,
    ) -> (String, tokio::task::JoinHandle<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind download test server");
        let addr = listener.local_addr().expect("download test server addr");
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.expect("accept download request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let read = stream
                    .read(&mut buffer)
                    .await
                    .expect("read download request");
                if read == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..read]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }

            let mut response = format!("HTTP/1.1 {status}\r\nconnection: close\r\n");
            if let Some(content_length) = content_length {
                response.push_str(&format!("content-length: {content_length}\r\n"));
            }
            response.push_str("\r\n");
            stream
                .write_all(response.as_bytes())
                .await
                .expect("write response headers");
            stream.write_all(&body).await.expect("write response body");
        });
        (format!("http://{addr}/artifact.bin"), server)
    }

    fn has_fact(facts: &[crate::execution::ExecutionFact], kind: ExecutionFactKind) -> bool {
        facts.iter().any(|fact| fact.kind == kind)
    }

    fn fact_has_field(
        facts: &[crate::execution::ExecutionFact],
        kind: ExecutionFactKind,
        key: &str,
        value: &str,
    ) -> bool {
        facts.iter().any(|fact| {
            fact.kind == kind
                && fact
                    .fields
                    .iter()
                    .any(|field| field.key == key && field.value == value)
        })
    }

    fn assert_no_temp_files(root: &Path, destination: &Path) {
        let entries = match fs::read_dir(root) {
            Ok(entries) => entries
                .collect::<Result<Vec<_>, _>>()
                .expect("read entries"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => panic!("read root entries: {error}"),
        };
        let leftovers = entries
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| path != destination)
            .collect::<Vec<_>>();
        assert!(
            leftovers.is_empty(),
            "unexpected leftover files: {leftovers:?}"
        );
    }

    fn assert_no_raw_path_or_url(
        facts: &[crate::execution::ExecutionFact],
        root: &Path,
        url: &str,
    ) {
        let facts_json = serde_json::to_string(facts).expect("serialize facts");
        assert!(!facts_json.contains(root.to_string_lossy().as_ref()));
        assert!(!facts_json.contains(url));
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha256::digest(bytes))
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
    }

    fn launcher_target(id: &str) -> TargetDescriptor {
        TargetDescriptor::new(
            StabilizationSystem::Execution,
            TargetKind::Artifact,
            id,
            OwnershipClass::LauncherManaged,
        )
    }

    fn test_root(prefix: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-download-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    fn cleanup(root: &Path) {
        let _ = fs::remove_dir_all(root);
    }
}
