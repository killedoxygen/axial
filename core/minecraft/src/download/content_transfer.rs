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
use crate::loaders::types::LoaderError;
use crate::managed_fs::{AnchoredDirectory, ManagedDir, ManagedFileGuard};
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

#[derive(Clone, Copy)]
enum StagingDestination<'a> {
    Legacy(&'a Path),
    Owned {
        directory: &'a AnchoredDirectory,
        filename: &'a str,
    },
}

impl<'a> StagingDestination<'a> {
    fn label_path(self) -> &'a Path {
        match self {
            Self::Legacy(path) => path,
            Self::Owned { filename, .. } => Path::new(filename),
        }
    }

    fn is_owned(self) -> bool {
        matches!(self, Self::Owned { .. })
    }
}

#[derive(Debug, thiserror::Error)]
pub enum VerifiedStagedContentError {
    #[error("verified staged content publication identity is invalid")]
    InvalidPublication,
    #[error(
        "verified staged content publication may have taken effect and requires reconciliation"
    )]
    PublicationIndeterminate,
    #[error("verified staged content filesystem operation failed: {0}")]
    Io(#[source] io::Error),
}

/// An identity-bound provider artifact that has not yet transferred ownership.
///
/// The bytes live in an anonymous or delete-on-close file. Dropping this value
/// closes that exact file without deleting through a caller-visible pathname.
#[derive(Debug)]
pub struct VerifiedStagedContent {
    staged: OwnedStagingFile,
    sha512: String,
    report: ExecutionDownloadReport,
}

impl VerifiedStagedContent {
    pub fn file_name(&self) -> &str {
        self.staged.name()
    }

    pub fn size(&self) -> u64 {
        self.staged.guard().size()
    }

    pub fn sha512(&self) -> &str {
        &self.sha512
    }

    pub fn report(&self) -> &ExecutionDownloadReport {
        &self.report
    }

    /// Atomically create a hard link from the retained file handle.
    ///
    /// [`VerifiedStagedContentError::PublicationIndeterminate`] means the link
    /// syscall succeeded but post-effect verification or directory sync failed.
    pub fn publish_create_new(
        mut self,
        published_directory: &AnchoredDirectory,
        filename: &str,
    ) -> Result<ExecutionDownloadReport, VerifiedStagedContentError> {
        self.staged
            .publish_create_new(published_directory, filename, &self.sha512)?;
        Ok(self.report)
    }

    pub fn discard(mut self) -> Result<ExecutionDownloadReport, VerifiedStagedContentError> {
        self.staged.discard()?;
        Ok(self.report)
    }
}

struct CompletedStaging {
    staged: OwnedStagingFile,
    sha512: Option<String>,
    report: ExecutionDownloadReport,
}

struct OwnedStagingFile {
    directory: ManagedDir,
    name: String,
    anonymous: bool,
    guard: Option<ManagedFileGuard>,
}

impl std::fmt::Debug for OwnedStagingFile {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("OwnedStagingFile")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl OwnedStagingFile {
    fn create(destination: StagingDestination<'_>) -> Result<(Self, std::fs::File), io::Error> {
        let (directory, name, anonymous) = match destination {
            StagingDestination::Legacy(path) => {
                let parent = path
                    .parent()
                    .filter(|value| !value.as_os_str().is_empty())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "content staging destination has no explicit parent",
                        )
                    })?;
                let name = path
                    .file_name()
                    .and_then(|value| value.to_str())
                    .ok_or_else(|| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "content staging filename is invalid",
                        )
                    })?
                    .to_string();
                let directory = ManagedDir::open_root(parent).map_err(managed_staging_io_error)?;
                (directory, name, false)
            }
            StagingDestination::Owned {
                directory,
                filename,
            } => (directory.managed_directory(), filename.to_string(), true),
        };
        let guard = if anonymous {
            directory
                .create_anonymous_guarded_file()
                .map_err(managed_staging_io_error)?
        } else {
            directory
                .create_new_guarded_file(&name)
                .map_err(managed_staging_io_error)?
        };
        let staged = Self {
            directory,
            name,
            anonymous,
            guard: Some(guard),
        };
        let writer = staged
            .guard()
            .try_clone_file()
            .map_err(managed_staging_io_error)?;
        Ok((staged, writer))
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn guard(&self) -> &ManagedFileGuard {
        self.guard
            .as_ref()
            .expect("owned staging file retains its guard until settlement")
    }

    fn guard_mut(&mut self) -> &mut ManagedFileGuard {
        self.guard
            .as_mut()
            .expect("owned staging file retains its guard until settlement")
    }

    fn settle(&mut self, expected_size: u64) -> Result<(), io::Error> {
        let observed_size = self
            .guard_mut()
            .capture_size()
            .map_err(managed_staging_io_error)?;
        if observed_size != expected_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "content staging file identity changed before settlement",
            ));
        }
        Ok(())
    }

    fn publish_create_new(
        &mut self,
        published_directory: &AnchoredDirectory,
        name: &str,
        expected_sha512: &str,
    ) -> Result<(), VerifiedStagedContentError> {
        if !self.anonymous {
            return Err(VerifiedStagedContentError::InvalidPublication);
        }
        published_directory
            .validate_child_name(name)
            .map_err(VerifiedStagedContentError::Io)?;
        let destination = published_directory.managed_directory();
        destination
            .link_guarded_file_no_replace(self.guard(), name)
            .map_err(publication_error)?;
        let verification = (|| {
            self.guard()
                .settle_anonymous_publication()
                .map_err(publication_error)?;
            let observed = destination
                .inspect_regular_file(name)
                .map_err(publication_error)?
                .ok_or(VerifiedStagedContentError::InvalidPublication)?;
            if !destination
                .has_portably_exact_child_name(name)
                .map_err(publication_error)?
                || observed.size() != self.guard().size()
                || !destination
                    .file_guard_matches(name, self.guard())
                    .map_err(publication_error)?
                || destination
                    .sha512_guarded_file(name, self.guard(), MAX_VERIFIED_CONTENT_STAGING_BYTES)
                    .map_err(publication_error)?
                    != expected_sha512
            {
                return Err(VerifiedStagedContentError::InvalidPublication);
            }
            destination.sync().map_err(publication_error)
        })();
        self.guard = None;
        verification.map_err(|_| VerifiedStagedContentError::PublicationIndeterminate)
    }

    fn discard(&mut self) -> Result<(), VerifiedStagedContentError> {
        let Some(guard) = self.guard.as_ref() else {
            return Ok(());
        };
        if !self.anonymous {
            self.directory
                .remove_guarded_file(&self.name, guard)
                .map_err(publication_error)?;
        }
        self.guard = None;
        Ok(())
    }

    fn release_to_legacy_caller(&mut self) {
        self.guard = None;
    }
}

impl Drop for OwnedStagingFile {
    fn drop(&mut self) {
        let _ = self.discard();
    }
}

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

/// Download a provider artifact into a create-only, identity-bound staging file.
///
/// This stricter cross-crate primitive requires an exact positive size and a
/// canonical lowercase SHA-512 digest. Ownership remains armed until the
/// caller transfers ownership with an exact create-only handle publication.
pub async fn download_owned_verified_content_to_staging(
    client: &reqwest::Client,
    url: &str,
    staging_directory: &AnchoredDirectory,
    filename: &str,
    expected: &VerifiedContentIntegrity,
) -> Result<VerifiedStagedContent, ExecutionDownloadError> {
    let staging_destination = Path::new(filename);
    let target = safe_download_target_label(staging_destination);
    validate_owned_content_integrity(expected, staging_destination, &target)?;
    staging_directory
        .validate_child_name(filename)
        .map_err(|error| staging_io_error(&target, error))?;
    let retry_delays = CONTENT_DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis);
    let completed = download_verified_content_to_staging_owned_with_retry_delays(
        client,
        url,
        StagingDestination::Owned {
            directory: staging_directory,
            filename,
        },
        expected,
        &retry_delays,
    )
    .await?;
    Ok(VerifiedStagedContent {
        staged: completed.staged,
        sha512: completed
            .sha512
            .expect("owned verified staging requires a SHA-512 digest"),
        report: completed.report,
    })
}

async fn download_verified_content_to_staging_with_retry_delays(
    client: &reqwest::Client,
    url: &str,
    staging_destination: &Path,
    expected: &VerifiedContentIntegrity,
    retry_delays: &[Duration],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let mut completed = download_verified_content_to_staging_owned_with_retry_delays(
        client,
        url,
        StagingDestination::Legacy(staging_destination),
        expected,
        retry_delays,
    )
    .await?;
    completed.staged.release_to_legacy_caller();
    Ok(completed.report)
}

async fn download_verified_content_to_staging_owned_with_retry_delays(
    client: &reqwest::Client,
    url: &str,
    staging_destination: StagingDestination<'_>,
    expected: &VerifiedContentIntegrity,
    retry_delays: &[Duration],
) -> Result<CompletedStaging, ExecutionDownloadError> {
    let label_path = staging_destination.label_path();
    let target = safe_download_target_label(label_path);
    validate_content_integrity(expected, label_path, &target)?;

    let mut prior_facts = Vec::new();
    let mut next_delay = 0_usize;
    loop {
        match download_verified_content_attempt(client, url, staging_destination, expected, &target)
            .await
        {
            Ok(mut report) => {
                prior_facts.append(&mut report.report.facts);
                report.report.facts = prior_facts;
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

fn validate_owned_content_integrity(
    expected: &VerifiedContentIntegrity,
    staging_destination: &Path,
    target: &str,
) -> Result<(), ExecutionDownloadError> {
    validate_content_integrity(expected, staging_destination, target)?;
    let Some(size) = expected.size else {
        return Err(metadata_error(
            staging_destination,
            target,
            "size",
            "is missing an exact positive size",
        ));
    };
    if size == 0 {
        return Err(metadata_error(
            staging_destination,
            target,
            "size",
            "has an invalid exact size",
        ));
    }
    let Some(sha512) = expected.sha512.as_deref() else {
        return Err(metadata_error(
            staging_destination,
            target,
            "sha512",
            "is missing an exact sha512 checksum",
        ));
    };
    if sha512.len() != 128
        || !sha512
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(metadata_error(
            staging_destination,
            target,
            "sha512",
            "has a non-canonical sha512 checksum",
        ));
    }
    Ok(())
}

fn metadata_error(
    staging_destination: &Path,
    target: &str,
    field: &'static str,
    detail: &str,
) -> ExecutionDownloadError {
    execution_download_error(
        ExecutionDownloadFactKind::MetadataInvalid,
        vec![execution_download_fact(
            ExecutionDownloadFactKind::MetadataInvalid,
            target,
            vec![("field", field)],
        )],
        content_integrity_error(staging_destination, detail),
    )
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

async fn validate_legacy_staging_destination(
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
    staging_destination: StagingDestination<'_>,
    expected: &VerifiedContentIntegrity,
    target: &str,
) -> Result<CompletedStaging, ExecutionDownloadError> {
    let label_path = staging_destination.label_path();
    let mut facts = Vec::new();
    if let StagingDestination::Legacy(path) = staging_destination {
        validate_legacy_staging_destination(path, target).await?;
    }
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
            download_size_mismatch(label_path, byte_limit, content_length),
        ));
    }

    let (mut staged, output) = OwnedStagingFile::create(staging_destination)
        .map_err(|error| staging_io_error(target, error))?;
    let mut output = tokio::fs::File::from_std(output);

    let mut sha1 = expected.sha1.is_some().then(Sha1::new);
    let mut streamed_sha512 = expected.sha512.is_some().then(Sha512::new);
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
                discard_failed_staging_file(&mut staged, target, &mut facts);
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
            discard_failed_staging_file(&mut staged, target, &mut facts);
            return Err(execution_download_error(
                ExecutionDownloadFactKind::SizeMismatch,
                facts,
                download_size_mismatch(label_path, byte_limit, next_written),
            ));
        }
        if let Some(hasher) = sha1.as_mut() {
            hasher.update(&chunk);
        }
        if let Some(hasher) = streamed_sha512.as_mut() {
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
            discard_failed_staging_file(&mut staged, target, &mut facts);
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
        discard_failed_staging_file(&mut staged, target, &mut facts);
        return Err(execution_download_error(
            ExecutionDownloadFactKind::Interrupted,
            facts,
            DownloadError::Integrity(format!(
                "{} download ended before its declared size",
                bounded_download_file_label(label_path)
            )),
        ));
    }
    let sync_result = match output.flush().await {
        Ok(()) if staging_destination.is_owned() => output.sync_all().await,
        Ok(()) => Ok(()),
        Err(error) => Err(error),
    };
    if let Err(error) = sync_result {
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
        discard_failed_staging_file(&mut staged, target, &mut facts);
        return Err(execution_download_error(
            execution_io_fact_kind(error.kind()),
            facts,
            DownloadError::FileOperation(error),
        ));
    }
    drop(output);
    let actual_sha1 = sha1.map(|hasher| format!("{:x}", hasher.finalize()));
    let actual_sha512 = streamed_sha512.map(|hasher| format!("{:x}", hasher.finalize()));
    if staging_destination.is_owned() {
        let settled = tokio::task::spawn_blocking(move || {
            let result = staged.settle(written);
            (staged, result)
        })
        .await;
        let (returned_staged, settlement) = match settled {
            Ok(settled) => settled,
            Err(_) => {
                let error = io::Error::other("managed content staging settlement task stopped");
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::TempWriteFailed,
                    target,
                    no_download_fact_fields(),
                ));
                return Err(execution_download_error(
                    ExecutionDownloadFactKind::TempWriteFailed,
                    facts,
                    DownloadError::FileOperation(error),
                ));
            }
        };
        staged = returned_staged;
        if let Err(error) = settlement {
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
            discard_failed_staging_file(&mut staged, target, &mut facts);
            return Err(execution_download_error(
                execution_io_fact_kind(error.kind()),
                facts,
                DownloadError::FileOperation(error),
            ));
        }
    }

    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let actual = actual_sha1
            .as_deref()
            .expect("retained sha1 exists when sha1 is expected");
        if !actual.eq_ignore_ascii_case(expected_sha1) {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha1")],
            ));
            discard_failed_staging_file(&mut staged, target, &mut facts);
            return Err(execution_download_error(
                ExecutionDownloadFactKind::ChecksumMismatch,
                facts,
                content_integrity_error(label_path, "failed sha1 verification"),
            ));
        }
    }
    let actual_sha512 = if let Some(expected_sha512) = expected.sha512.as_deref() {
        let actual = actual_sha512.expect("retained sha512 exists when sha512 is expected");
        if !actual.eq_ignore_ascii_case(expected_sha512) {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha512")],
            ));
            discard_failed_staging_file(&mut staged, target, &mut facts);
            return Err(execution_download_error(
                ExecutionDownloadFactKind::ChecksumMismatch,
                facts,
                content_integrity_error(label_path, "failed sha512 verification"),
            ));
        }
        Some(actual)
    } else {
        None
    };

    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        target,
        vec![("bytes", written.to_string())],
    ));
    Ok(CompletedStaging {
        staged,
        sha512: actual_sha512,
        report: ExecutionDownloadReport {
            target: target.to_string(),
            bytes_written: written,
            facts,
        },
    })
}

fn discard_failed_staging_file(
    staged: &mut OwnedStagingFile,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
) {
    match staged.discard() {
        Ok(()) => facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempDiscarded,
            target,
            no_download_fact_fields(),
        )),
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

fn managed_staging_io_error(error: LoaderError) -> io::Error {
    match error {
        LoaderError::Io(error) => error,
        _ => io::Error::new(
            io::ErrorKind::InvalidData,
            "managed content staging identity is invalid",
        ),
    }
}

fn publication_error(error: LoaderError) -> VerifiedStagedContentError {
    match error {
        LoaderError::Io(error) => VerifiedStagedContentError::Io(error),
        _ => VerifiedStagedContentError::InvalidPublication,
    }
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

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_requires_positive_size_and_canonical_sha512_before_network() {
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let body = b"owned content";
        let mut missing_size = integrity(body);
        missing_size.size = None;
        let missing_size_error = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            "http://127.0.0.1:9/unreachable",
            &directory,
            "content.jar",
            &missing_size,
        )
        .await
        .expect_err("exact size is required");
        assert_eq!(
            missing_size_error.kind,
            ExecutionDownloadFactKind::MetadataInvalid
        );

        let mut uppercase_sha512 = integrity(body);
        uppercase_sha512.sha512 = uppercase_sha512.sha512.map(|value| value.to_uppercase());
        let uppercase_error = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            "http://127.0.0.1:9/unreachable",
            &directory,
            "content.jar",
            &uppercase_sha512,
        )
        .await
        .expect_err("canonical lowercase sha512 is required");
        assert_eq!(
            uppercase_error.kind,
            ExecutionDownloadFactKind::MetadataInvalid
        );
        let invalid_name = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            "http://127.0.0.1:9/unreachable",
            &directory,
            "../content.jar",
            &integrity(body),
        )
        .await
        .expect_err("owned staging filename must be one validated segment");
        assert_eq!(
            invalid_name.kind,
            ExecutionDownloadFactKind::TempWriteFailed
        );
        assert_eq!(
            invalid_name.io_error_kind(),
            Some(io::ErrorKind::InvalidInput)
        );
        assert!(!root.path().join("content.jar").exists());
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_evidence_needs_no_crash_recovery_namespace() {
        let body = b"owned content".to_vec();
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let staged = owned_stage(&directory, "content.jar", &body).await;

        assert_eq!(staged.file_name(), "content.jar");
        assert_eq!(staged.size(), body.len() as u64);
        assert_eq!(staged.sha512(), integrity(&body).sha512.unwrap());
        assert_eq!(staged.report().bytes_written, body.len() as u64);
        #[cfg(target_os = "linux")]
        assert!(
            root.path()
                .read_dir()
                .expect("root entries")
                .next()
                .is_none()
        );
        drop(staged);

        assert!(
            root.path()
                .read_dir()
                .expect("root entries")
                .next()
                .is_none()
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_hash_and_size_mismatches_clean_exact_stage() {
        let body = b"provider bytes".to_vec();
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(vec![(200, body.clone())], Arc::clone(&attempts)).await;
        let mut wrong_size = integrity(&body);
        wrong_size.size = Some(body.len() as u64 - 1);
        let size_error = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            &url,
            &directory,
            "size.jar",
            &wrong_size,
        )
        .await
        .expect_err("size mismatch");
        assert_eq!(size_error.kind, ExecutionDownloadFactKind::SizeMismatch);
        assert!(!root.path().join("size.jar").exists());

        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(vec![(200, body.clone())], Arc::clone(&attempts)).await;
        let wrong_hash = integrity(b"different byte");
        let hash_error = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            &url,
            &directory,
            "hash.jar",
            &wrong_hash,
        )
        .await
        .expect_err("sha512 mismatch");
        assert_eq!(hash_error.kind, ExecutionDownloadFactKind::ChecksumMismatch);
        assert!(!root.path().join("hash.jar").exists());
        assert!(
            root.path()
                .read_dir()
                .expect("root entries")
                .next()
                .is_none()
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_publishes_the_exact_handle_create_new() {
        let body = b"published content".to_vec();
        let root = TempDir::new().expect("staging root");
        let source = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let destination = source
            .open_or_create_child("nested")
            .expect("anchor nested publication root");
        let staged = owned_stage(&source, "content.stage", &body).await;
        let published = root.path().join("nested/content.jar");

        let report = staged
            .publish_create_new(&destination, "content.jar")
            .expect("exact create-only publication");

        assert_eq!(report.bytes_written, body.len() as u64);
        assert_eq!(std::fs::read(published).expect("published bytes"), body);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_final_window_collision_preserves_replacement() {
        let body = b"owned content".to_vec();
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let staged = owned_stage(&directory, "content.stage", &body).await;
        let published = root.path().join("content.jar");
        std::fs::write(&published, b"replacement").expect("destination replacement");

        staged
            .publish_create_new(&directory, "content.jar")
            .expect_err("create-only publication collision");

        assert_eq!(
            std::fs::read(&published).expect("replacement preserved"),
            b"replacement"
        );
        assert_eq!(root.path().read_dir().expect("root entries").count(), 1);
    }

    #[cfg(any(target_os = "linux", windows))]
    #[tokio::test]
    async fn owned_staging_rejects_portable_alias_without_publication() {
        let body = b"owned content".to_vec();
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let staged = owned_stage(&directory, "content.stage", &body).await;
        let alias = root.path().join("Content.JAR");
        std::fs::write(&alias, b"alias").expect("portable alias");

        staged
            .publish_create_new(&directory, "content.jar")
            .expect_err("portable alias must fail closed");

        assert_eq!(std::fs::read(alias).expect("alias preserved"), b"alias");
        assert_eq!(root.path().read_dir().expect("root entries").count(), 1);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn owned_staging_refuses_symlink_publication() {
        let body = b"owned content".to_vec();
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let staged = owned_stage(&directory, "content.stage", &body).await;
        let published = root.path().join("content.jar");
        std::os::unix::fs::symlink("target.jar", &published).expect("publication symlink");

        staged
            .publish_create_new(&directory, "content.jar")
            .expect_err("symlink cannot receive publication");
        assert!(published.is_symlink());
        assert_eq!(root.path().read_dir().expect("root entries").count(), 1);
    }

    #[cfg(all(unix, not(target_os = "linux")))]
    #[tokio::test]
    async fn owned_staging_fails_closed_without_anonymous_inode_support() {
        let body = b"owned content";
        let root = TempDir::new().expect("staging root");
        let directory = AnchoredDirectory::open(root.path()).expect("anchor staging root");
        let error = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            "http://127.0.0.1:9/unreachable",
            &directory,
            "content.jar",
            &integrity(body),
        )
        .await
        .expect_err("unsupported Unix platform");

        assert_eq!(error.kind, ExecutionDownloadFactKind::TempWriteFailed);
        assert!(
            root.path()
                .read_dir()
                .expect("root entries")
                .next()
                .is_none()
        );
    }

    #[cfg(any(target_os = "linux", windows))]
    async fn owned_stage(
        directory: &AnchoredDirectory,
        name: &str,
        body: &[u8],
    ) -> VerifiedStagedContent {
        let attempts = Arc::new(AtomicUsize::new(0));
        let url = serve_responses(vec![(200, body.to_vec())], Arc::clone(&attempts)).await;
        let staged = download_owned_verified_content_to_staging(
            &reqwest::Client::new(),
            &url,
            directory,
            name,
            &integrity(body),
        )
        .await
        .expect("owned verified stage");
        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        staged
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
