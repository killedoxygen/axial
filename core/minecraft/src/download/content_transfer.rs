use super::facts::{
    execution_download_error, execution_download_fact, no_download_fact_fields,
    size_mismatch_fact,
};
use super::model::{
    DownloadError, ExecutionDownloadError, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExecutionDownloadReport, VerifiedContentIntegrity,
};
use super::path_safety::{bounded_download_file_label, safe_download_target_label};
use axial_fs::{
    Directory, FileCreateOutcome, FileCreateResolution, FilePromotionOutcome, LeafName,
    SealedStagedFile, StageDiscardOutcome, StageDiscardResolution, StagedFile,
};
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use sha2::Sha512;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

pub const MAX_VERIFIED_CONTENT_STAGING_BYTES: u64 = 1 << 30;

const CONTENT_DOWNLOAD_RETRY_DELAY_MILLIS: [u64; 3] = [500, 1_500, 4_000];
const STAGE_STREAM_CAPACITY: usize = 8;

#[derive(Debug, thiserror::Error)]
pub enum VerifiedStagedContentError {
    #[error("verified staged content publication identity is invalid")]
    InvalidPublication,
    #[error("verified staged content publication remains unsettled: {0}")]
    PublicationIndeterminate(#[source] io::Error),
    #[error("verified staged content filesystem operation failed: {0}")]
    Io(#[source] io::Error),
}

/// A verified provider artifact retained as an unpublished axial-fs stage.
#[must_use = "verified staged content must be published or explicitly discarded"]
pub struct VerifiedStagedContent {
    staged: Option<SealedStagedFile>,
    source_directory: Directory,
    filename: String,
    size: u64,
    sha512: String,
    report: ExecutionDownloadReport,
}

impl std::fmt::Debug for VerifiedStagedContent {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VerifiedStagedContent")
            .field("filename", &self.filename)
            .field("size", &self.size)
            .finish_non_exhaustive()
    }
}

impl VerifiedStagedContent {
    pub fn file_name(&self) -> &str {
        &self.filename
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn sha512(&self) -> &str {
        &self.sha512
    }

    pub fn report(&self) -> &ExecutionDownloadReport {
        &self.report
    }

    pub fn publish_create_new(
        mut self,
        published_directory: &Directory,
        filename: &str,
    ) -> Result<ExecutionDownloadReport, VerifiedStagedContentError> {
        let name = verified_leaf(filename)?;
        let staged = self
            .staged
            .take()
            .ok_or(VerifiedStagedContentError::InvalidPublication)?;
        let current = match staged.promote_no_replace(
            &self.source_directory,
            published_directory,
            &name,
        ) {
            FilePromotionOutcome::Applied(file) => file,
            FilePromotionOutcome::NoEffect { error, staged } => {
                discard_sealed(staged).map_err(VerifiedStagedContentError::Io)?;
                return Err(VerifiedStagedContentError::Io(error));
            }
            FilePromotionOutcome::AppliedUnverified(obligation) => {
                return Err(VerifiedStagedContentError::PublicationIndeterminate(
                    io::Error::other(RetainedPromotion {
                        obligation: Some(obligation),
                    }),
                ));
            }
        };
        let revision = current
            .revision()
            .map_err(VerifiedStagedContentError::Io)?;
        if revision.size() != self.size {
            return Err(VerifiedStagedContentError::PublicationIndeterminate(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "published verified content changed size",
                ),
            ));
        }
        let observed = sha512_file(&current, &revision, self.size)
            .map_err(VerifiedStagedContentError::PublicationIndeterminate)?;
        if observed != self.sha512 {
            return Err(VerifiedStagedContentError::PublicationIndeterminate(
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "published verified content changed digest",
                ),
            ));
        }
        published_directory
            .sync()
            .map_err(VerifiedStagedContentError::PublicationIndeterminate)?;
        Ok(self.report.clone())
    }

    pub fn discard(mut self) -> Result<ExecutionDownloadReport, VerifiedStagedContentError> {
        if let Some(staged) = self.staged.take() {
            discard_sealed(staged).map_err(VerifiedStagedContentError::Io)?;
        }
        Ok(self.report.clone())
    }
}

impl Drop for VerifiedStagedContent {
    fn drop(&mut self) {
        let Some(staged) = self.staged.take() else {
            return;
        };
        if let Err(error) = discard_sealed(staged) {
            if error
                .get_ref()
                .is_some_and(|source| source.is::<RetainedStageDiscard>())
            {
                drop(error);
            } else {
                std::process::abort();
            }
        }
    }
}

struct RetainedPromotion {
    obligation: Option<axial_fs::FilePromotionObligation>,
}

impl std::fmt::Debug for RetainedPromotion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RetainedPromotion")
            .finish_non_exhaustive()
    }
}

impl std::fmt::Display for RetainedPromotion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("file promotion obligation is retained")
    }
}

impl std::error::Error for RetainedPromotion {}

impl Drop for RetainedPromotion {
    fn drop(&mut self) {
        let Some(obligation) = self.obligation.take() else {
            return;
        };
        match obligation.reconcile() {
            axial_fs::FilePromotionResolution::Applied(_) => {}
            axial_fs::FilePromotionResolution::NoEffect(staged) => {
                if discard_sealed(staged).is_err() {
                    std::process::abort();
                }
            }
            axial_fs::FilePromotionResolution::Indeterminate(obligation) => {
                std::mem::forget(obligation);
                std::process::abort();
            }
        }
    }
}

enum StageMessage {
    Bytes(Vec<u8>),
    Abort,
}

pub async fn download_owned_verified_content_to_staging(
    client: &reqwest::Client,
    url: &str,
    staging_directory: &Directory,
    filename: &str,
    expected: &VerifiedContentIntegrity,
) -> Result<VerifiedStagedContent, ExecutionDownloadError> {
    let label_path = Path::new(filename);
    let target = safe_download_target_label(label_path);
    validate_owned_content_integrity(expected, label_path, &target)?;
    verified_leaf(filename).map_err(|_| {
        staging_io_error(
            &target,
            io::Error::new(io::ErrorKind::InvalidInput, "content filename is invalid"),
        )
    })?;
    let retry_delays = CONTENT_DOWNLOAD_RETRY_DELAY_MILLIS.map(Duration::from_millis);
    let mut prior_facts = Vec::new();
    let mut next_delay = 0_usize;
    loop {
        match download_verified_content_attempt(
            client,
            url,
            staging_directory,
            filename,
            expected,
            &target,
        )
        .await
        {
            Ok(mut staged) => {
                prior_facts.append(&mut staged.report.facts);
                staged.report.facts = prior_facts;
                return Ok(staged);
            }
            Err(mut error)
                if next_delay < retry_delays.len()
                    && execution_content_download_error_is_retryable(&error) =>
            {
                prior_facts.append(&mut error.facts);
                tokio::time::sleep(retry_delays[next_delay]).await;
                next_delay += 1;
            }
            Err(mut error) => {
                prior_facts.append(&mut error.facts);
                error.facts = prior_facts;
                return Err(error);
            }
        }
    }
}

async fn download_verified_content_attempt(
    client: &reqwest::Client,
    url: &str,
    staging_directory: &Directory,
    filename: &str,
    expected: &VerifiedContentIntegrity,
    target: &str,
) -> Result<VerifiedStagedContent, ExecutionDownloadError> {
    let label_path = Path::new(filename);
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

    let expected_size = expected.size.expect("owned content has an exact size");
    if response
        .content_length()
        .is_some_and(|declared| declared != expected_size)
    {
        let declared = response.content_length().unwrap_or_default();
        facts.push(size_mismatch_fact(target, expected_size, declared));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::SizeMismatch,
            facts,
            download_size_mismatch(label_path, expected_size, declared),
        ));
    }

    let staged = create_stage(staging_directory)
        .map_err(|error| staging_io_error(target, error))?;
    let (sender, receiver) = tokio::sync::mpsc::channel(STAGE_STREAM_CAPACITY);
    let writer = tokio::task::spawn_blocking(move || write_stage(staged, receiver));
    let mut sha1 = expected.sha1.is_some().then(Sha1::new);
    let mut sha512 = Sha512::new();
    let mut written = 0_u64;
    let mut stream = response.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = match chunk {
            Ok(chunk) => chunk,
            Err(error) => {
                let _ = sender.send(StageMessage::Abort).await;
                drop(sender);
                settle_failed_writer(writer, target, &mut facts).await;
                facts.push(execution_download_fact(
                    ExecutionDownloadFactKind::NetworkFailure,
                    target,
                    no_download_fact_fields(),
                ));
                return Err(execution_download_error(
                    ExecutionDownloadFactKind::Interrupted,
                    facts,
                    DownloadError::Request(error),
                ));
            }
        };
        let Some(next) = written.checked_add(chunk.len() as u64) else {
            let _ = sender.send(StageMessage::Abort).await;
            drop(sender);
            settle_failed_writer(writer, target, &mut facts).await;
            return Err(staging_io_error(
                target,
                io::Error::other("content byte count overflowed"),
            ));
        };
        if next > expected_size {
            let _ = sender.send(StageMessage::Abort).await;
            drop(sender);
            settle_failed_writer(writer, target, &mut facts).await;
            facts.push(size_mismatch_fact(target, expected_size, next));
            return Err(execution_download_error(
                ExecutionDownloadFactKind::SizeMismatch,
                facts,
                download_size_mismatch(label_path, expected_size, next),
            ));
        }
        if let Some(hasher) = sha1.as_mut() {
            hasher.update(&chunk);
        }
        sha512.update(&chunk);
        if sender.send(StageMessage::Bytes(chunk.to_vec())).await.is_err() {
            drop(sender);
            settle_failed_writer(writer, target, &mut facts).await;
            return Err(staging_io_error(
                target,
                io::Error::other("content staging writer stopped"),
            ));
        }
        written = next;
    }
    drop(sender);
    let sealed = match writer.await {
        Ok(Ok(Some(sealed))) => sealed,
        Ok(Ok(None)) => {
            return Err(staging_io_error(
                target,
                io::Error::other("content staging writer aborted unexpectedly"),
            ));
        }
        Ok(Err(error)) => return Err(staging_io_error(target, error)),
        Err(_) => {
            return Err(staging_io_error(
                target,
                io::Error::other("content staging writer task stopped"),
            ));
        }
    };

    if written != expected_size {
        discard_sealed(sealed).map_err(|error| staging_io_error(target, error))?;
        facts.push(size_mismatch_fact(target, expected_size, written));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::Interrupted,
            facts,
            download_size_mismatch(label_path, expected_size, written),
        ));
    }
    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let actual = format!("{:x}", sha1.expect("SHA-1 state is retained").finalize());
        if !actual.eq_ignore_ascii_case(expected_sha1) {
            discard_sealed(sealed).map_err(|error| staging_io_error(target, error))?;
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha1")],
            ));
            return Err(execution_download_error(
                ExecutionDownloadFactKind::ChecksumMismatch,
                facts,
                content_integrity_error(label_path, "failed sha1 verification"),
            ));
        }
    }
    let actual_sha512 = format!("{:x}", sha512.finalize());
    if !actual_sha512.eq_ignore_ascii_case(
        expected
            .sha512
            .as_deref()
            .expect("owned content has an exact SHA-512"),
    ) {
        discard_sealed(sealed).map_err(|error| staging_io_error(target, error))?;
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            target,
            vec![("algorithm", "sha512")],
        ));
        return Err(execution_download_error(
            ExecutionDownloadFactKind::ChecksumMismatch,
            facts,
            content_integrity_error(label_path, "failed sha512 verification"),
        ));
    }
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        target,
        vec![("bytes", written.to_string())],
    ));
    Ok(VerifiedStagedContent {
        staged: Some(sealed),
        source_directory: staging_directory.clone(),
        filename: filename.to_string(),
        size: written,
        sha512: actual_sha512,
        report: ExecutionDownloadReport {
            target: target.to_string(),
            bytes_written: written,
            facts,
        },
    })
}

fn create_stage(directory: &Directory) -> io::Result<StagedFile> {
    match directory.create_stage() {
        FileCreateOutcome::Created(staged) => Ok(staged),
        FileCreateOutcome::NoEffect(error) => Err(error),
        FileCreateOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            FileCreateResolution::Created(staged) => Ok(staged),
            FileCreateResolution::Indeterminate(obligation) => {
                Err(io::Error::other(RetainedStageCreate {
                    obligation: Some(obligation),
                }))
            }
        },
    }
}

fn write_stage(
    mut staged: StagedFile,
    mut receiver: tokio::sync::mpsc::Receiver<StageMessage>,
) -> io::Result<Option<SealedStagedFile>> {
    let transfer = (|| -> io::Result<bool> {
        let mut writer = staged.writer()?;
        while let Some(message) = receiver.blocking_recv() {
            match message {
                StageMessage::Bytes(bytes) => writer.write_all(&bytes)?,
                StageMessage::Abort => return Ok(false),
            }
        }
        writer.finish()?;
        Ok(true)
    })();
    match transfer {
        Ok(true) => staged.seal().map(Some).map_err(|failure| {
            let error = io::Error::new(failure.error().kind(), failure.error().to_string());
            let staged = failure.into_staged();
            discard_staged(staged).unwrap_or_else(|_| std::process::abort());
            error
        }),
        Ok(false) => {
            discard_staged(staged)?;
            Ok(None)
        }
        Err(error) => {
            discard_staged(staged)?;
            Err(error)
        }
    }
}

async fn settle_failed_writer(
    writer: tokio::task::JoinHandle<io::Result<Option<SealedStagedFile>>>,
    target: &str,
    facts: &mut Vec<ExecutionDownloadFact>,
) {
    match writer.await {
        Ok(Ok(None)) => facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempDiscarded,
            target,
            no_download_fact_fields(),
        )),
        Ok(Ok(Some(sealed))) => {
            let _ = discard_sealed(sealed);
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                no_download_fact_fields(),
            ));
        }
        _ => facts.push(execution_download_fact(
            ExecutionDownloadFactKind::TempWriteFailed,
            target,
            no_download_fact_fields(),
        )),
    }
}

fn discard_staged(staged: StagedFile) -> io::Result<()> {
    settle_discard(staged.discard())
}

fn discard_sealed(staged: SealedStagedFile) -> io::Result<()> {
    settle_discard(staged.discard())
}

fn settle_discard(outcome: StageDiscardOutcome) -> io::Result<()> {
    match outcome {
        StageDiscardOutcome::Discarded => Ok(()),
        StageDiscardOutcome::AppliedUnverified(obligation) => match obligation.reconcile() {
            StageDiscardResolution::Discarded => Ok(()),
            StageDiscardResolution::Indeterminate(obligation) => {
                Err(io::Error::other(RetainedStageDiscard {
                    obligation: Some(obligation),
                }))
            }
        },
    }
}

struct RetainedStageCreate {
    obligation: Option<axial_fs::FileCreateObligation>,
}

struct RetainedStageDiscard {
    obligation: Option<axial_fs::StageDiscardObligation>,
}

macro_rules! retained_effect_error {
    ($type:ty, $message:literal) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.debug_struct(stringify!($type)).finish_non_exhaustive()
            }
        }

        impl std::fmt::Display for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str($message)
            }
        }

        impl std::error::Error for $type {}
    };
}

retained_effect_error!(RetainedStageCreate, "stage create obligation is retained");
retained_effect_error!(RetainedStageDiscard, "stage discard obligation is retained");

impl Drop for RetainedStageCreate {
    fn drop(&mut self) {
        let Some(obligation) = self.obligation.take() else {
            return;
        };
        match obligation.reconcile() {
            FileCreateResolution::Created(staged) => {
                if discard_staged(staged).is_err() {
                    std::process::abort();
                }
            }
            FileCreateResolution::Indeterminate(obligation) => {
                std::mem::forget(obligation);
                std::process::abort();
            }
        }
    }
}

impl Drop for RetainedStageDiscard {
    fn drop(&mut self) {
        let Some(obligation) = self.obligation.take() else {
            return;
        };
        if let StageDiscardResolution::Indeterminate(obligation) = obligation.reconcile() {
            std::mem::forget(obligation);
            std::process::abort();
        }
    }
}

fn sha512_file(
    file: &axial_fs::FileCapability,
    revision: &axial_fs::FileRevision,
    size: u64,
) -> io::Result<String> {
    let mut reader = file.reader(size)?;
    let mut hasher = Sha512::new();
    let mut observed = 0_u64;
    let mut chunk = [0_u8; 64 * 1024];
    loop {
        let read = std::io::Read::read(&mut reader, &mut chunk)?;
        if read == 0 {
            break;
        }
        observed = observed
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("verified content size overflowed"))?;
        hasher.update(&chunk[..read]);
    }
    reader.finish()?;
    file.validate_revision(revision)?;
    if observed != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "verified content changed size while hashing",
        ));
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn verified_leaf(filename: &str) -> Result<LeafName, VerifiedStagedContentError> {
    crate::portable_path::PortableFileName::new_exact(filename)
        .map_err(|_| VerifiedStagedContentError::InvalidPublication)?;
    LeafName::new(filename).map_err(|_| VerifiedStagedContentError::InvalidPublication)
}

fn validate_owned_content_integrity(
    expected: &VerifiedContentIntegrity,
    staging_destination: &Path,
    target: &str,
) -> Result<(), ExecutionDownloadError> {
    let Some(size) = expected.size else {
        return Err(metadata_error(
            staging_destination,
            target,
            "size",
            "is missing an exact positive size",
        ));
    };
    if size == 0 || size > MAX_VERIFIED_CONTENT_STAGING_BYTES {
        return Err(metadata_error(
            staging_destination,
            target,
            "size",
            "has an invalid exact size",
        ));
    }
    if expected
        .sha1
        .as_deref()
        .is_some_and(|digest| !valid_hex_digest(digest, 40))
    {
        return Err(metadata_error(
            staging_destination,
            target,
            "sha1",
            "has a non-canonical sha1 checksum",
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

fn staging_io_error(target: &str, error: io::Error) -> ExecutionDownloadError {
    let kind = if error.kind() == io::ErrorKind::PermissionDenied {
        ExecutionDownloadFactKind::PermissionFailure
    } else {
        ExecutionDownloadFactKind::TempWriteFailed
    };
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
        "{} size mismatch: expected {expected}, got {actual}",
        bounded_download_file_label(destination)
    ))
}
