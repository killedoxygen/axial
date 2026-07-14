use super::facts::{
    emit_execution_download_facts, execution_download_fact, no_download_fact_fields,
    size_mismatch_fact,
};
use super::model::{
    DownloadError, ExactLibraryDownloadProof, ExecutionDownloadFact, ExecutionDownloadFactKind,
    ExpectedIntegrity,
};
use crate::artifact_path::ArtifactRelativePath;
use crate::known_good::MAX_TIER2_AGGREGATE_BYTES;
use crate::loaders::types::LoaderError;
use crate::managed_fs::ManagedDir;
use futures_util::StreamExt as _;
use sha1::{Digest as _, Sha1};
use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

pub(super) const LIBRARY_SOURCE_MAX_BYTES: u64 = 512 << 20;
const LIBRARY_SOURCE_BUDGET_UNIT_BYTES: u64 = 1 << 20;
const LIBRARY_SOURCE_BUDGET_UNITS: u32 =
    (LIBRARY_SOURCE_MAX_BYTES / LIBRARY_SOURCE_BUDGET_UNIT_BYTES) as u32;
const LIBRARY_SOURCE_RETRY_DELAYS: [Duration; 3] = [
    Duration::from_millis(500),
    Duration::from_millis(1_500),
    Duration::from_millis(4_000),
];
const MAX_JAR_ENTRIES: u16 = 32_768;
const MAX_JAR_CENTRAL_DIRECTORY_BYTES: u32 = 16 << 20;
const MAX_JAR_ENTRY_NAME_BYTES: usize = 1 << 10;
const MAX_JAR_TOTAL_NAME_BYTES: usize = 8 << 20;
const MAX_JAR_ENTRY_UNCOMPRESSED_BYTES: u64 = LIBRARY_SOURCE_MAX_BYTES;
const MAX_JAR_TOTAL_UNCOMPRESSED_BYTES: u64 = LIBRARY_SOURCE_MAX_BYTES;
const ZIP_END_OF_CENTRAL_DIRECTORY_BYTES: usize = 22;
const ZIP_MAX_COMMENT_BYTES: usize = u16::MAX as usize;

#[derive(Clone)]
pub(super) struct LibrarySourcePool {
    acquisition_permits: Arc<Semaphore>,
    retention: LibrarySourceRetention,
}

#[derive(Clone)]
enum LibrarySourceRetention {
    DirectWriter,
    Component { budget: Arc<RetainedSourceBudget> },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LibrarySourceRetentionKind {
    DirectWriter,
    Component,
}

struct RetainedSourceBudget {
    available_bytes: Mutex<u64>,
}

struct RetainedSourcePermit {
    budget: Arc<RetainedSourceBudget>,
    bytes: u64,
}

pub(super) struct LibrarySourcePermit(LibrarySourcePermitInner);

enum LibrarySourcePermitInner {
    DirectWriter { _permit: OwnedSemaphorePermit },
    Component { permit: RetainedSourcePermit },
}

impl RetainedSourceBudget {
    fn new(bytes: u64) -> Arc<Self> {
        Arc::new(Self {
            available_bytes: Mutex::new(bytes),
        })
    }

    fn try_reserve(self: &Arc<Self>, bytes: u64) -> Result<RetainedSourcePermit, DownloadError> {
        let mut available = self
            .available_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available = available
            .checked_sub(bytes)
            .ok_or_else(|| source_integrity_error("exceeds the aggregate retained-source limit"))?;
        Ok(RetainedSourcePermit {
            budget: Arc::clone(self),
            bytes,
        })
    }

    #[cfg(test)]
    fn available_bytes(&self) -> u64 {
        *self
            .available_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl Drop for RetainedSourcePermit {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available_bytes
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *available = available
            .checked_add(self.bytes)
            .expect("retained library source budget overflow");
    }
}

impl LibrarySourcePool {
    pub(super) fn new() -> Self {
        Self {
            acquisition_permits: Arc::new(Semaphore::new(LIBRARY_SOURCE_BUDGET_UNITS as usize)),
            retention: LibrarySourceRetention::DirectWriter,
        }
    }

    pub(super) fn for_component_retention() -> Self {
        Self::for_component_retention_bytes(MAX_TIER2_AGGREGATE_BYTES)
    }

    fn for_component_retention_bytes(retained_bytes: u64) -> Self {
        Self {
            acquisition_permits: Arc::new(Semaphore::new(LIBRARY_SOURCE_BUDGET_UNITS as usize)),
            retention: LibrarySourceRetention::Component {
                budget: RetainedSourceBudget::new(retained_bytes),
            },
        }
    }

    async fn reserve(&self, hard_limit: u64) -> Result<OwnedSemaphorePermit, DownloadError> {
        if hard_limit == 0 || hard_limit > LIBRARY_SOURCE_MAX_BYTES {
            return Err(source_integrity_error("exceeds the bounded scratch limit"));
        }
        let units = hard_limit.div_ceil(LIBRARY_SOURCE_BUDGET_UNIT_BYTES) as u32;
        Arc::clone(&self.acquisition_permits)
            .acquire_many_owned(units)
            .await
            .map_err(|_| source_integrity_error("scratch budget is closed"))
    }

    fn retain_validated(
        &self,
        observed_size: u64,
        acquisition_permit: OwnedSemaphorePermit,
    ) -> Result<(LibrarySourcePermit, LibrarySourceRetentionKind), DownloadError> {
        let LibrarySourceRetention::Component { budget } = &self.retention else {
            return Ok((
                LibrarySourcePermit(LibrarySourcePermitInner::DirectWriter {
                    _permit: acquisition_permit,
                }),
                LibrarySourceRetentionKind::DirectWriter,
            ));
        };
        let retained_permit = budget.try_reserve(observed_size)?;
        drop(acquisition_permit);
        Ok((
            LibrarySourcePermit(LibrarySourcePermitInner::Component {
                permit: retained_permit,
            }),
            LibrarySourceRetentionKind::Component,
        ))
    }

    #[cfg(test)]
    fn available_bytes(&self) -> u64 {
        self.acquisition_permits.available_permits() as u64 * LIBRARY_SOURCE_BUDGET_UNIT_BYTES
    }

    #[cfg(test)]
    fn retained_available_bytes(&self) -> Option<u64> {
        match &self.retention {
            LibrarySourceRetention::DirectWriter => None,
            LibrarySourceRetention::Component { budget } => Some(budget.available_bytes()),
        }
    }
}

pub(super) struct AuthenticatedLibrarySource {
    file: File,
    relative_path: ArtifactRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    expected: ExpectedIntegrity,
    target: String,
    provider_url: String,
    _permit: LibrarySourcePermit,
    retention_kind: LibrarySourceRetentionKind,
}

impl AuthenticatedLibrarySource {
    #[cfg(test)]
    pub(super) fn file(&self) -> &File {
        &self.file
    }

    #[cfg(test)]
    pub(super) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    #[cfg(test)]
    pub(super) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    pub(super) fn expected(&self) -> &ExpectedIntegrity {
        &self.expected
    }

    #[cfg(test)]
    pub(super) fn target(&self) -> &str {
        &self.target
    }

    pub(super) fn relative_path(&self) -> &ArtifactRelativePath {
        &self.relative_path
    }

    pub(super) fn provider_url(&self) -> &str {
        &self.provider_url
    }

    #[cfg(test)]
    pub(super) fn observed_expected(&self) -> ExpectedIntegrity {
        ExpectedIntegrity {
            size: Some(self.observed_size),
            sha1: Some(format!("{}", HexSha1(&self.observed_sha1))),
        }
    }

    pub(super) fn into_parts(
        self,
    ) -> (
        File,
        ArtifactRelativePath,
        u64,
        [u8; 20],
        ExpectedIntegrity,
        String,
        String,
        LibrarySourcePermit,
    ) {
        (
            self.file,
            self.relative_path,
            self.observed_size,
            self.observed_sha1,
            self.expected,
            self.target,
            self.provider_url,
            self._permit,
        )
    }

    pub(super) fn into_exact_download_proof(self, is_native: bool) -> ExactLibraryDownloadProof {
        ExactLibraryDownloadProof::new(
            self.relative_path,
            is_native,
            self.provider_url,
            self.expected,
            self.observed_size,
            self.observed_sha1,
        )
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LibraryComponentSourceKind {
    Library,
    NativeLibrary,
}

pub(crate) struct RetainedLibraryComponentSource {
    _file: File,
    relative_path: ArtifactRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    expected: ExpectedIntegrity,
    provider_url: String,
    kind: LibraryComponentSourceKind,
    _permit: RetainedSourcePermit,
}

pub(crate) struct StagedLibraryComponentSource {
    relative_path: ArtifactRelativePath,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: LibraryComponentSourceKind,
}

impl RetainedLibraryComponentSource {
    pub(crate) fn relative_path(&self) -> &ArtifactRelativePath {
        &self.relative_path
    }

    pub(crate) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    pub(crate) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }

    pub(crate) fn expected(&self) -> &ExpectedIntegrity {
        &self.expected
    }

    pub(crate) fn provider_url(&self) -> &str {
        &self.provider_url
    }

    pub(super) fn kind(&self) -> LibraryComponentSourceKind {
        self.kind
    }

    pub(crate) fn exact_download_proof(&self) -> ExactLibraryDownloadProof {
        ExactLibraryDownloadProof::new(
            self.relative_path.clone(),
            self.kind == LibraryComponentSourceKind::NativeLibrary,
            self.provider_url.clone(),
            self.expected.clone(),
            self.observed_size,
            self.observed_sha1,
        )
    }

    pub(crate) async fn stage_create_new(
        self,
        staging: &ManagedDir,
        slot: &ArtifactRelativePath,
    ) -> Result<StagedLibraryComponentSource, LoaderError> {
        let Self {
            _file: file,
            relative_path,
            observed_size,
            observed_sha1,
            expected: _,
            provider_url: _,
            kind,
            _permit: permit,
        } = self;
        staging
            .import_relative_authenticated_create_new(
                slot,
                file,
                observed_size,
                observed_sha1,
                permit,
            )
            .await?;
        Ok(StagedLibraryComponentSource {
            relative_path,
            observed_size,
            observed_sha1,
            kind,
        })
    }

    #[cfg(test)]
    async fn stage_create_new_with_hook(
        self,
        staging: &ManagedDir,
        slot: &ArtifactRelativePath,
        blocking_hook: BlockingValidationHook,
    ) -> Result<StagedLibraryComponentSource, LoaderError> {
        let Self {
            _file: file,
            relative_path,
            observed_size,
            observed_sha1,
            expected: _,
            provider_url: _,
            kind,
            _permit: permit,
        } = self;
        staging
            .import_relative_authenticated_create_new_with_hook(
                slot,
                file,
                observed_size,
                observed_sha1,
                permit,
                blocking_hook,
            )
            .await?;
        Ok(StagedLibraryComponentSource {
            relative_path,
            observed_size,
            observed_sha1,
            kind,
        })
    }
}

impl StagedLibraryComponentSource {
    pub(crate) fn relative_path(&self) -> &ArtifactRelativePath {
        &self.relative_path
    }

    pub(super) fn kind(&self) -> LibraryComponentSourceKind {
        self.kind
    }

    pub(crate) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    pub(crate) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }
}

pub(super) struct LibrarySourceRequest<'a> {
    pub(super) client: &'a reqwest::Client,
    pub(super) url: &'a str,
    pub(super) expected: &'a ExpectedIntegrity,
    pub(super) relative_path: &'a ArtifactRelativePath,
    pub(super) max_bytes: u64,
    pub(super) target: &'a str,
    pub(super) pool: &'a LibrarySourcePool,
    pub(super) fact_tx: Option<&'a mpsc::UnboundedSender<ExecutionDownloadFact>>,
}

pub(super) async fn acquire_authenticated_library_source(
    request: LibrarySourceRequest<'_>,
) -> Result<AuthenticatedLibrarySource, DownloadError> {
    acquire_authenticated_library_source_with_retry_delays(request, &LIBRARY_SOURCE_RETRY_DELAYS)
        .await
}

pub(super) async fn acquire_retained_library_component_source(
    request: LibrarySourceRequest<'_>,
    kind: LibraryComponentSourceKind,
) -> Result<RetainedLibraryComponentSource, DownloadError> {
    if !matches!(
        &request.pool.retention,
        LibrarySourceRetention::Component { .. }
    ) {
        return Err(source_integrity_error(
            "component retention requires a component source pool",
        ));
    }
    let source = acquire_authenticated_library_source(request).await?;
    if source.retention_kind != LibrarySourceRetentionKind::Component {
        return Err(source_integrity_error(
            "component source did not retain its aggregate budget",
        ));
    }
    let LibrarySourcePermit(LibrarySourcePermitInner::Component { permit }) = source._permit else {
        return Err(source_integrity_error(
            "component source retained the wrong budget kind",
        ));
    };
    Ok(RetainedLibraryComponentSource {
        _file: source.file,
        relative_path: source.relative_path,
        observed_size: source.observed_size,
        observed_sha1: source.observed_sha1,
        expected: source.expected,
        provider_url: source.provider_url,
        kind,
        _permit: permit,
    })
}

async fn acquire_authenticated_library_source_with_retry_delays(
    request: LibrarySourceRequest<'_>,
    retry_delays: &[Duration],
) -> Result<AuthenticatedLibrarySource, DownloadError> {
    let mut next_delay = 0;
    loop {
        match acquire_authenticated_library_source_attempt(&request).await {
            Ok(source) => return Ok(source),
            Err(error) if error.retryable && next_delay < retry_delays.len() => {
                tokio::time::sleep(retry_delays[next_delay]).await;
                next_delay += 1;
            }
            Err(error) => return Err(error.error),
        }
    }
}

struct LibrarySourceAttemptError {
    error: DownloadError,
    retryable: bool,
}

fn emit_source_facts<const N: usize>(
    request: &LibrarySourceRequest<'_>,
    facts: [ExecutionDownloadFact; N],
) {
    emit_execution_download_facts(request.fact_tx, &facts);
}

fn emit_source_fact<K, V, I>(
    request: &LibrarySourceRequest<'_>,
    kind: ExecutionDownloadFactKind,
    fields: I,
) where
    K: AsRef<str>,
    V: AsRef<str>,
    I: IntoIterator<Item = (K, V)>,
{
    emit_source_facts(
        request,
        [execution_download_fact(kind, request.target, fields)],
    );
}

fn emit_source_size_mismatch(request: &LibrarySourceRequest<'_>, expected: u64, actual: u64) {
    emit_source_facts(
        request,
        [size_mismatch_fact(request.target, expected, actual)],
    );
}

async fn acquire_authenticated_library_source_attempt(
    request: &LibrarySourceRequest<'_>,
) -> Result<AuthenticatedLibrarySource, LibrarySourceAttemptError> {
    acquire_authenticated_library_source_attempt_inner(request, None).await
}

type BlockingValidationHook = Box<dyn FnOnce() + Send + 'static>;

async fn acquire_authenticated_library_source_attempt_inner(
    request: &LibrarySourceRequest<'_>,
    validation_hook: Option<BlockingValidationHook>,
) -> Result<AuthenticatedLibrarySource, LibrarySourceAttemptError> {
    if request
        .expected
        .sha1
        .as_deref()
        .is_some_and(|sha1| sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit()))
    {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::MetadataInvalid,
            vec![("field", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "checksum metadata is invalid",
        )));
    }
    let hard_limit = request
        .expected
        .size
        .unwrap_or(LIBRARY_SOURCE_MAX_BYTES)
        .min(request.max_bytes)
        .min(LIBRARY_SOURCE_MAX_BYTES);
    if request.expected.size == Some(0)
        || request
            .expected
            .size
            .is_some_and(|size| size > request.max_bytes || size > LIBRARY_SOURCE_MAX_BYTES)
    {
        let actual = request.expected.size.unwrap_or(request.max_bytes);
        emit_source_size_mismatch(
            request,
            request.max_bytes.min(LIBRARY_SOURCE_MAX_BYTES),
            actual,
        );
        return Err(nonretryable(source_integrity_error(
            "exceeds the bounded scratch limit",
        )));
    }
    let permit = request
        .pool
        .reserve(hard_limit)
        .await
        .map_err(nonretryable)?;
    let response = request.client.get(request.url).send().await.map_err(|_| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::NetworkFailure,
            no_download_fact_fields(),
        );
        retryable("request failed")
    })?;
    if !response.status().is_success() {
        let status = response.status();
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ProviderFailure,
            vec![("status", status.as_u16().to_string())],
        );
        return Err(LibrarySourceAttemptError {
            error: source_integrity_error("provider rejected the request"),
            retryable: status.is_server_error() || status.as_u16() == 429,
        });
    }
    let declared_length = response.content_length();
    if declared_length.is_some_and(|length| length > hard_limit) {
        emit_source_size_mismatch(request, hard_limit, declared_length.unwrap_or(hard_limit));
        return Err(nonretryable(source_integrity_error(
            "declared response exceeds the bounded scratch limit",
        )));
    }

    let file = tempfile::tempfile().map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    let mut output = tokio::fs::File::from_std(file);
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut observed_size = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|_| {
            emit_source_facts(
                request,
                [
                    execution_download_fact(
                        ExecutionDownloadFactKind::Interrupted,
                        request.target,
                        no_download_fact_fields(),
                    ),
                    execution_download_fact(
                        ExecutionDownloadFactKind::NetworkFailure,
                        request.target,
                        no_download_fact_fields(),
                    ),
                ],
            );
            retryable("response stream was interrupted")
        })?;
        observed_size = observed_size
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| nonretryable(source_integrity_error("source size overflow")))?;
        if observed_size > hard_limit {
            emit_source_size_mismatch(request, hard_limit, observed_size);
            return Err(nonretryable(source_integrity_error(
                "response exceeds the bounded scratch limit",
            )));
        }
        hasher.update(&chunk);
        output.write_all(&chunk).await.map_err(|error| {
            emit_source_fact(
                request,
                ExecutionDownloadFactKind::TempWriteFailed,
                no_download_fact_fields(),
            );
            nonretryable(error.into())
        })?;
    }
    if declared_length.is_some_and(|length| length != observed_size) {
        let declared = declared_length.unwrap_or(observed_size);
        emit_source_facts(
            request,
            [
                execution_download_fact(
                    ExecutionDownloadFactKind::Interrupted,
                    request.target,
                    no_download_fact_fields(),
                ),
                size_mismatch_fact(request.target, declared, observed_size),
            ],
        );
        return Err(retryable("response stream was interrupted"));
    }
    output.flush().await.map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    output.sync_data().await.map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(error.into())
    })?;
    let observed_sha1: [u8; 20] = hasher.finalize().into();
    if let Some(expected_size) = request.expected.size
        && expected_size != observed_size
    {
        emit_source_size_mismatch(request, expected_size, observed_size);
        return Err(nonretryable(source_integrity_error(
            "source size does not match metadata",
        )));
    }
    if let Some(expected_sha1) = request.expected.sha1.as_deref()
        && !expected_sha1.eq_ignore_ascii_case(&format!("{}", HexSha1(&observed_sha1)))
    {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ChecksumMismatch,
            vec![("algorithm", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "source checksum does not match metadata",
        )));
    }

    let mut file = output.into_std().await;
    let verified = tokio::task::spawn_blocking(move || {
        if let Some(hook) = validation_hook {
            hook();
        }
        let verified_sha1 = hash_open_file(&mut file, observed_size)?;
        validate_bounded_jar(&file)?;
        file.seek(SeekFrom::Start(0))?;
        Ok::<_, std::io::Error>((file, verified_sha1, permit))
    })
    .await
    .map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::TempWriteFailed,
            no_download_fact_fields(),
        );
        nonretryable(std::io::Error::other(error.to_string()).into())
    })?
    .map_err(|error| {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ChecksumMismatch,
            vec![("algorithm", "sha1")],
        );
        nonretryable(error.into())
    })?;
    let (file, verified_sha1, permit) = verified;
    if verified_sha1 != observed_sha1 {
        emit_source_fact(
            request,
            ExecutionDownloadFactKind::ChecksumMismatch,
            vec![("algorithm", "sha1")],
        );
        return Err(nonretryable(source_integrity_error(
            "retained source identity changed after download",
        )));
    }

    let (permit, retention_kind) = request
        .pool
        .retain_validated(observed_size, permit)
        .map_err(nonretryable)?;

    Ok(AuthenticatedLibrarySource {
        file,
        relative_path: request.relative_path.clone(),
        observed_size,
        observed_sha1,
        expected: request.expected.clone(),
        target: request.target.to_string(),
        provider_url: request.url.to_string(),
        _permit: permit,
        retention_kind,
    })
}

fn hash_open_file(file: &mut File, expected_size: u64) -> std::io::Result<[u8; 20]> {
    if file.metadata()?.len() != expected_size {
        return Err(std::io::Error::other(
            "retained library source size changed",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    if observed != expected_size {
        return Err(std::io::Error::other(
            "retained library source read was incomplete",
        ));
    }
    file.seek(SeekFrom::Start(0))?;
    Ok(hasher.finalize().into())
}

fn validate_bounded_jar(file: &File) -> std::io::Result<()> {
    preflight_zip_central_directory(file)?;
    let mut archive = zip::ZipArchive::new(file.try_clone()?)?;
    if archive.is_empty() || archive.len() > usize::from(MAX_JAR_ENTRIES) {
        return Err(std::io::Error::other(
            "library JAR entry count exceeds the bounded limit",
        ));
    }
    let mut total_name_bytes = 0_usize;
    let mut total_uncompressed_bytes = 0_u64;
    let mut has_file = false;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let name_bytes = entry.name_raw().len();
        if name_bytes > MAX_JAR_ENTRY_NAME_BYTES {
            return Err(std::io::Error::other("library JAR entry name is too large"));
        }
        total_name_bytes = total_name_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| std::io::Error::other("library JAR name budget overflow"))?;
        if total_name_bytes > MAX_JAR_TOTAL_NAME_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry names exceed the bounded limit",
            ));
        }
        if entry.size() > MAX_JAR_ENTRY_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry expands beyond the bounded limit",
            ));
        }
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(entry.size())
            .ok_or_else(|| std::io::Error::other("library JAR size budget overflow"))?;
        if total_uncompressed_bytes > MAX_JAR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR expands beyond the bounded limit",
            ));
        }
        has_file |= !entry.is_dir();
    }
    if !has_file {
        return Err(std::io::Error::other(
            "library source is not a readable non-empty JAR",
        ));
    }
    Ok(())
}

fn preflight_zip_central_directory(file: &File) -> std::io::Result<()> {
    let mut file = file.try_clone()?;
    let length = file.metadata()?.len();
    let tail_len =
        length.min((ZIP_END_OF_CENTRAL_DIRECTORY_BYTES + ZIP_MAX_COMMENT_BYTES) as u64) as usize;
    file.seek(SeekFrom::End(-(tail_len as i64)))?;
    let mut tail = vec![0_u8; tail_len];
    file.read_exact(&mut tail)?;
    let eocd = tail
        .windows(4)
        .rposition(|bytes| bytes == [0x50, 0x4b, 0x05, 0x06])
        .ok_or_else(|| std::io::Error::other("library source has no ZIP directory"))?;
    if tail.len().saturating_sub(eocd) < ZIP_END_OF_CENTRAL_DIRECTORY_BYTES {
        return Err(std::io::Error::other("library ZIP directory is truncated"));
    }
    let disk = u16::from_le_bytes([tail[eocd + 4], tail[eocd + 5]]);
    let directory_disk = u16::from_le_bytes([tail[eocd + 6], tail[eocd + 7]]);
    let disk_entries = u16::from_le_bytes([tail[eocd + 8], tail[eocd + 9]]);
    let entries = u16::from_le_bytes([tail[eocd + 10], tail[eocd + 11]]);
    let directory_bytes = u32::from_le_bytes(tail[eocd + 12..eocd + 16].try_into().unwrap());
    let directory_offset = u32::from_le_bytes(tail[eocd + 16..eocd + 20].try_into().unwrap());
    let comment_bytes = u16::from_le_bytes([tail[eocd + 20], tail[eocd + 21]]) as usize;
    if eocd
        .checked_add(ZIP_END_OF_CENTRAL_DIRECTORY_BYTES)
        .and_then(|end| end.checked_add(comment_bytes))
        != Some(tail.len())
    {
        return Err(std::io::Error::other(
            "library ZIP directory does not end at physical EOF",
        ));
    }
    if disk != 0
        || directory_disk != 0
        || disk_entries != entries
        || entries == u16::MAX
        || directory_bytes == u32::MAX
        || directory_offset == u32::MAX
        || entries == 0
        || entries > MAX_JAR_ENTRIES
        || directory_bytes > MAX_JAR_CENTRAL_DIRECTORY_BYTES
    {
        return Err(std::io::Error::other(
            "library ZIP directory exceeds the bounded limit",
        ));
    }
    let eocd_offset = length
        .checked_sub(tail_len as u64)
        .and_then(|offset| offset.checked_add(eocd as u64))
        .ok_or_else(|| std::io::Error::other("library ZIP directory offset overflow"))?;
    if u64::from(directory_offset).checked_add(u64::from(directory_bytes)) != Some(eocd_offset) {
        return Err(std::io::Error::other(
            "library ZIP central directory is not contiguous with its terminator",
        ));
    }

    file.seek(SeekFrom::Start(u64::from(directory_offset)))?;
    let mut consumed = 0_u64;
    let mut total_name_bytes = 0_usize;
    let mut total_uncompressed_bytes = 0_u64;
    let mut header = [0_u8; 46];
    for _ in 0..entries {
        file.read_exact(&mut header)?;
        consumed = consumed
            .checked_add(header.len() as u64)
            .ok_or_else(|| std::io::Error::other("library ZIP directory size overflow"))?;
        if header[0..4] != [0x50, 0x4b, 0x01, 0x02] {
            return Err(std::io::Error::other(
                "library ZIP central header signature is invalid",
            ));
        }
        let compressed = u32::from_le_bytes(header[20..24].try_into().unwrap());
        let uncompressed = u32::from_le_bytes(header[24..28].try_into().unwrap());
        let name_bytes = u16::from_le_bytes(header[28..30].try_into().unwrap()) as usize;
        let extra_bytes = u16::from_le_bytes(header[30..32].try_into().unwrap()) as u64;
        let entry_comment_bytes = u16::from_le_bytes(header[32..34].try_into().unwrap()) as u64;
        let start_disk = u16::from_le_bytes(header[34..36].try_into().unwrap());
        let local_offset = u32::from_le_bytes(header[42..46].try_into().unwrap());
        if compressed == u32::MAX
            || uncompressed == u32::MAX
            || local_offset == u32::MAX
            || start_disk != 0
            || name_bytes == 0
        {
            return Err(std::io::Error::other(
                "library ZIP central header exceeds the bounded format",
            ));
        }
        if name_bytes > MAX_JAR_ENTRY_NAME_BYTES {
            return Err(std::io::Error::other("library JAR entry name is too large"));
        }
        if u64::from(uncompressed) > MAX_JAR_ENTRY_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry expands beyond the bounded limit",
            ));
        }
        total_name_bytes = total_name_bytes
            .checked_add(name_bytes)
            .ok_or_else(|| std::io::Error::other("library ZIP name budget overflow"))?;
        total_uncompressed_bytes = total_uncompressed_bytes
            .checked_add(u64::from(uncompressed))
            .ok_or_else(|| std::io::Error::other("library ZIP size budget overflow"))?;
        if total_name_bytes > MAX_JAR_TOTAL_NAME_BYTES {
            return Err(std::io::Error::other(
                "library JAR entry names exceed the bounded limit",
            ));
        }
        if total_uncompressed_bytes > MAX_JAR_TOTAL_UNCOMPRESSED_BYTES {
            return Err(std::io::Error::other(
                "library JAR expands beyond the bounded limit",
            ));
        }
        let variable_bytes = (name_bytes as u64)
            .checked_add(extra_bytes)
            .and_then(|bytes| bytes.checked_add(entry_comment_bytes))
            .ok_or_else(|| std::io::Error::other("library ZIP header length overflow"))?;
        consumed = consumed
            .checked_add(variable_bytes)
            .ok_or_else(|| std::io::Error::other("library ZIP directory size overflow"))?;
        if consumed > u64::from(directory_bytes) {
            return Err(std::io::Error::other(
                "library ZIP central directory is truncated",
            ));
        }
        file.seek(SeekFrom::Current(i64::try_from(variable_bytes).map_err(
            |_| std::io::Error::other("library ZIP header skip is too large"),
        )?))?;
    }
    if consumed != u64::from(directory_bytes) {
        return Err(std::io::Error::other(
            "library ZIP central directory count does not match its size",
        ));
    }
    Ok(())
}

struct HexSha1<'a>(&'a [u8; 20]);

impl std::fmt::Display for HexSha1<'_> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

fn retryable(message: &'static str) -> LibrarySourceAttemptError {
    LibrarySourceAttemptError {
        error: source_integrity_error(message),
        retryable: true,
    }
}

fn nonretryable(error: DownloadError) -> LibrarySourceAttemptError {
    LibrarySourceAttemptError {
        error,
        retryable: false,
    }
}

fn source_integrity_error(message: &str) -> DownloadError {
    DownloadError::Integrity(format!("library source {message}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::Sha1;
    use std::collections::VecDeque;
    use std::io::Write as _;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Condvar, Mutex};
    use tokio::io::AsyncReadExt as _;
    use tokio::sync::oneshot;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    struct ScriptedResponse {
        status: &'static str,
        content_length: Option<usize>,
        body: Vec<u8>,
        started: Option<oneshot::Sender<()>>,
        body_gate: Option<oneshot::Receiver<()>>,
    }

    impl ScriptedResponse {
        fn full(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(body.len()),
                body,
                started: None,
                body_gate: None,
            }
        }

        fn partial(content_length: usize, body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(content_length),
                body,
                started: None,
                body_gate: None,
            }
        }

        fn without_length(body: Vec<u8>) -> Self {
            Self {
                status: "200 OK",
                content_length: None,
                body,
                started: None,
                body_gate: None,
            }
        }

        fn status(status: &'static str) -> Self {
            Self {
                status,
                content_length: Some(0),
                body: Vec::new(),
                started: None,
                body_gate: None,
            }
        }

        fn gated(
            body: Vec<u8>,
            started: oneshot::Sender<()>,
            body_gate: oneshot::Receiver<()>,
        ) -> Self {
            Self {
                status: "200 OK",
                content_length: Some(body.len()),
                body,
                started: Some(started),
                body_gate: Some(body_gate),
            }
        }
    }

    async fn spawn_server(responses: Vec<ScriptedResponse>) -> (String, Arc<AtomicUsize>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind library source server");
        let url = format!("http://{}/fixture.jar", listener.local_addr().unwrap());
        let requests = Arc::new(AtomicUsize::new(0));
        let requests_for_server = Arc::clone(&requests);
        tokio::spawn(async move {
            let mut responses = VecDeque::from(responses);
            while let Some(response) = responses.pop_front() {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                requests_for_server.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(write_response(socket, response));
            }
        });
        (url, requests)
    }

    async fn write_response(mut socket: tokio::net::TcpStream, mut response: ScriptedResponse) {
        let mut request = Vec::new();
        let mut buffer = [0_u8; 512];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let Ok(read) = socket.read(&mut buffer).await else {
                return;
            };
            if read == 0 {
                return;
            }
            request.extend_from_slice(&buffer[..read]);
        }
        let length = response
            .content_length
            .map(|length| format!("Content-Length: {length}\r\n"))
            .unwrap_or_default();
        let headers = format!(
            "HTTP/1.1 {}\r\n{length}Connection: close\r\n\r\n",
            response.status
        );
        if socket.write_all(headers.as_bytes()).await.is_err() {
            return;
        }
        if let Some(started) = response.started.take() {
            let _ = started.send(());
        }
        if let Some(gate) = response.body_gate.take() {
            let _ = gate.await;
        }
        let _ = socket.write_all(&response.body).await;
    }

    fn jar_bytes(payload: &[u8]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file("fixture.class", SimpleFileOptions::default())
            .expect("start JAR entry");
        writer.write_all(payload).expect("write JAR payload");
        writer.finish().expect("finish JAR").into_inner()
    }

    fn jar_with_entries(payloads: &[&[u8]]) -> Vec<u8> {
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (index, payload) in payloads.iter().enumerate() {
            writer
                .start_file(
                    format!("fixture-{index}.class"),
                    SimpleFileOptions::default(),
                )
                .expect("start JAR entry");
            writer.write_all(payload).expect("write JAR payload");
        }
        writer.finish().expect("finish JAR").into_inner()
    }

    fn patch_uncompressed_sizes(bytes: &mut [u8], sizes: &[u32]) {
        let mut local_index = 0;
        let mut central_index = 0;
        for offset in 0..bytes.len().saturating_sub(4) {
            match &bytes[offset..offset + 4] {
                [0x50, 0x4b, 0x03, 0x04] => {
                    bytes[offset + 22..offset + 26]
                        .copy_from_slice(&sizes[local_index].to_le_bytes());
                    local_index += 1;
                }
                [0x50, 0x4b, 0x01, 0x02] => {
                    bytes[offset + 24..offset + 28]
                        .copy_from_slice(&sizes[central_index].to_le_bytes());
                    central_index += 1;
                }
                _ => {}
            }
        }
        assert_eq!(local_index, sizes.len());
        assert_eq!(central_index, sizes.len());
    }

    async fn wait_for_requests(requests: &AtomicUsize, expected: usize) {
        for _ in 0..100 {
            if requests.load(Ordering::SeqCst) == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(requests.load(Ordering::SeqCst), expected);
    }

    fn sha1_bytes(bytes: &[u8]) -> [u8; 20] {
        Sha1::digest(bytes).into()
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        format!("{}", HexSha1(&sha1_bytes(bytes)))
    }

    async fn acquire(
        url: &str,
        expected: &ExpectedIntegrity,
        max_bytes: u64,
        target: &str,
        pool: &LibrarySourcePool,
    ) -> Result<AuthenticatedLibrarySource, DownloadError> {
        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        acquire_authenticated_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url,
                expected,
                relative_path: &relative_path,
                max_bytes,
                target,
                pool,
                fact_tx: None,
            },
            &[],
        )
        .await
    }

    async fn acquire_component(
        url: &str,
        expected: &ExpectedIntegrity,
        max_bytes: u64,
        target: &str,
        pool: &LibrarySourcePool,
        kind: LibraryComponentSourceKind,
    ) -> Result<RetainedLibraryComponentSource, DownloadError> {
        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        acquire_retained_library_component_source(
            LibrarySourceRequest {
                client: &client,
                url,
                expected,
                relative_path: &relative_path,
                max_bytes,
                target,
                pool,
                fact_tx: None,
            },
            kind,
        )
        .await
    }

    fn fixture_relative_path() -> ArtifactRelativePath {
        ArtifactRelativePath::new("org/example/fixture/1/fixture-1.jar")
            .expect("fixture relative path")
    }

    fn rejected(
        result: Result<AuthenticatedLibrarySource, DownloadError>,
        context: &str,
    ) -> DownloadError {
        result.err().unwrap_or_else(|| panic!("{context}"))
    }

    fn assert_source(
        source: &AuthenticatedLibrarySource,
        body: &[u8],
        expected: &ExpectedIntegrity,
        target: &str,
    ) {
        assert_eq!(source.observed_size(), body.len() as u64);
        assert_eq!(source.observed_sha1(), sha1_bytes(body));
        assert_eq!(source.expected(), expected);
        assert_eq!(source.target(), target);
        assert_eq!(source.relative_path(), &fixture_relative_path());
        assert_eq!(
            source.observed_expected(),
            ExpectedIntegrity {
                size: Some(body.len() as u64),
                sha1: Some(sha1_hex(body)),
            }
        );
        let mut file = source.file().try_clone().expect("clone retained source");
        file.seek(SeekFrom::Start(0)).expect("rewind source");
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).expect("read retained source");
        assert_eq!(actual, body);
    }

    #[tokio::test]
    async fn acquires_sha_only_exact_source_without_destination_authority() {
        let body = jar_bytes(b"sha-only");
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some(sha1_hex(&body)),
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();
        let absent_destination = std::env::temp_dir().join(format!(
            "axial-library-source-no-destination-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&absent_destination);

        let source = acquire(&url, &expected, body.len() as u64, "library:sha", &pool)
            .await
            .expect("authenticated SHA-only source");

        assert_source(&source, &body, &expected, "library:sha");
        assert!(!absent_destination.exists());
    }

    #[tokio::test]
    async fn acquires_size_only_exact_source() {
        let body = jar_bytes(b"size-only");
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();

        let source = acquire(&url, &expected, body.len() as u64, "library:size", &pool)
            .await
            .expect("authenticated size-only source");

        assert_source(&source, &body, &expected, "library:size");
    }

    #[tokio::test]
    async fn acquires_source_without_declared_integrity() {
        let body = jar_bytes(b"observed-only");
        let expected = ExpectedIntegrity::default();
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();

        let source = acquire(
            &url,
            &expected,
            body.len() as u64,
            "library:observed",
            &pool,
        )
        .await
        .expect("authenticated observed source");

        assert_source(&source, &body, &expected, "library:observed");
    }

    #[tokio::test]
    async fn invalid_sha_metadata_fails_before_network_or_budget_effects() {
        let body = jar_bytes(b"invalid-sha-metadata");
        let (url, requests) = spawn_server(vec![ScriptedResponse::full(body)]).await;
        let pool = LibrarySourcePool::new();
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some("not-a-sha1".to_string()),
        };

        let error = rejected(
            acquire(
                &url,
                &expected,
                LIBRARY_SOURCE_MAX_BYTES,
                "library:invalid-sha-metadata",
                &pool,
            )
            .await,
            "expected invalid SHA metadata rejection",
        );

        assert!(error.to_string().contains("checksum metadata is invalid"));
        assert_eq!(requests.load(Ordering::SeqCst), 0);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn expected_size_is_a_hard_limit_for_lengthless_streams() {
        let body = jar_bytes(b"larger-than-tiny-expectation");
        let expected = ExpectedIntegrity {
            size: Some(1),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::without_length(body)]).await;
        let pool = LibrarySourcePool::new();

        let error = rejected(
            acquire(
                &url,
                &expected,
                LIBRARY_SOURCE_MAX_BYTES,
                "library:tiny-contract",
                &pool,
            )
            .await,
            "expected stream hard-limit rejection",
        );

        assert!(error.to_string().contains("response exceeds"));
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn unknown_source_reserves_its_entire_request_cap() {
        const REQUEST_CAP: u64 = 17 << 20;
        let body = jar_bytes(b"small-source-large-contract");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();

        let source = acquire(
            &url,
            &ExpectedIntegrity::default(),
            REQUEST_CAP,
            "library:request-cap",
            &pool,
        )
        .await
        .expect("acquire unknown source");

        assert_source(
            &source,
            &body,
            &ExpectedIntegrity::default(),
            "library:request-cap",
        );
        assert_eq!(
            pool.available_bytes(),
            LIBRARY_SOURCE_MAX_BYTES - REQUEST_CAP
        );
        drop(source);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn component_retention_releases_acquisition_charge_and_uses_exact_observed_bytes() {
        let first_body = jar_bytes(b"first-checksumless-component-source");
        let second_body = jar_bytes(b"second-checksumless-component-source");
        let (url, _) = spawn_server(vec![
            ScriptedResponse::full(first_body.clone()),
            ScriptedResponse::full(second_body.clone()),
        ])
        .await;
        let pool = LibrarySourcePool::for_component_retention();

        let first = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:component-first",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("first retained component source");
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(
            pool.retained_available_bytes(),
            Some(MAX_TIER2_AGGREGATE_BYTES - first_body.len() as u64)
        );

        let second = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_component(
                &url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:component-second",
                &pool,
                LibraryComponentSourceKind::NativeLibrary,
            ),
        )
        .await
        .expect("second source must not deadlock on the acquisition budget")
        .expect("second retained component source");
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(
            pool.retained_available_bytes(),
            Some(MAX_TIER2_AGGREGATE_BYTES - first_body.len() as u64 - second_body.len() as u64)
        );

        drop((first, second));
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(
            pool.retained_available_bytes(),
            Some(MAX_TIER2_AGGREGATE_BYTES)
        );
    }

    #[tokio::test]
    async fn component_retention_fails_fast_when_exact_aggregate_budget_is_exhausted() {
        let body = jar_bytes(b"aggregate-overflow");
        let (url, requests) = spawn_server(vec![
            ScriptedResponse::full(body.clone()),
            ScriptedResponse::full(body.clone()),
        ])
        .await;
        let pool = LibrarySourcePool::for_component_retention_bytes(body.len() as u64);
        let first = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:aggregate-owner",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("aggregate owner");

        let result = tokio::time::timeout(
            Duration::from_secs(1),
            acquire_component(
                &url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:aggregate-overflow",
                &pool,
                LibraryComponentSourceKind::Library,
            ),
        )
        .await
        .expect("aggregate overflow must fail without waiting");
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("aggregate overflow must be rejected"),
        };

        assert!(
            error
                .to_string()
                .contains("aggregate retained-source limit")
        );
        assert_eq!(requests.load(Ordering::SeqCst), 2);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
        assert_eq!(pool.retained_available_bytes(), Some(0));
        drop(first);
        assert_eq!(pool.retained_available_bytes(), Some(body.len() as u64));
    }

    #[test]
    fn exact_retention_budget_accepts_the_maximum_count_of_one_byte_sources() {
        let budget = RetainedSourceBudget::new(crate::known_good::MAX_TIER2_ENTRIES as u64);
        let permits = (0..crate::known_good::MAX_TIER2_ENTRIES)
            .map(|_| budget.try_reserve(1).expect("one-byte retained source"))
            .collect::<Vec<_>>();

        assert_eq!(budget.available_bytes(), 0);
        assert!(budget.try_reserve(1).is_err());
        drop(permits);
        assert_eq!(
            budget.available_bytes(),
            crate::known_good::MAX_TIER2_ENTRIES as u64
        );
    }

    #[tokio::test]
    async fn retained_component_source_stages_create_only_and_derives_exact_proof() {
        let body = jar_bytes(b"component-staging");
        let (url, _) = spawn_server(vec![
            ScriptedResponse::full(body.clone()),
            ScriptedResponse::full(body.clone()),
        ])
        .await;
        let pool = LibrarySourcePool::for_component_retention();
        let source = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:component-staging",
            &pool,
            LibraryComponentSourceKind::NativeLibrary,
        )
        .await
        .expect("retained staging source");
        assert_eq!(source.relative_path(), &fixture_relative_path());
        assert_eq!(source.observed_size(), body.len() as u64);
        assert_eq!(source.observed_sha1(), sha1_bytes(&body));
        assert_eq!(source.expected(), &ExpectedIntegrity::default());
        assert_eq!(source.provider_url(), url);
        assert_eq!(source.kind(), LibraryComponentSourceKind::NativeLibrary);

        let temp = tempfile::tempdir().expect("component staging root");
        let staging = ManagedDir::open_root(temp.path()).expect("managed component staging root");
        let slot = ArtifactRelativePath::new("000000/000001").expect("deterministic staging slot");
        let proof = source.exact_download_proof();
        let staged = source
            .stage_create_new(&staging, &slot)
            .await
            .expect("stage retained component source");
        assert_eq!(staged.relative_path(), &fixture_relative_path());
        assert_eq!(staged.kind(), LibraryComponentSourceKind::NativeLibrary);
        assert_eq!(staged.observed_size(), body.len() as u64);
        assert_eq!(staged.observed_sha1(), sha1_bytes(&body));
        assert_eq!(
            std::fs::read(slot.join_under(temp.path())).expect("read staged source"),
            body
        );
        assert_eq!(
            pool.retained_available_bytes(),
            Some(MAX_TIER2_AGGREGATE_BYTES)
        );

        let (path, is_native, provider_url, expected, size, sha1) = proof.into_parts();
        assert_eq!(path, fixture_relative_path());
        assert!(is_native);
        assert_eq!(provider_url, url);
        assert_eq!(expected, ExpectedIntegrity::default());
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha1, sha1_bytes(&body));

        let occupied = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:component-occupied",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("retained occupied-slot source");
        let error = match occupied.stage_create_new(&staging, &slot).await {
            Err(error) => error,
            Ok(_) => panic!("occupied staging slot must fail closed"),
        };
        assert!(matches!(error, LoaderError::Io(_)));
        assert_eq!(
            std::fs::read(slot.join_under(temp.path())).expect("read unchanged staged source"),
            body
        );
        assert_eq!(
            pool.retained_available_bytes(),
            Some(MAX_TIER2_AGGREGATE_BYTES)
        );
    }

    #[tokio::test]
    async fn cancelled_staging_keeps_retained_budget_until_blocking_copy_finishes() {
        let body = jar_bytes(b"cancelled-component-staging");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::for_component_retention_bytes(body.len() as u64);
        let source = acquire_component(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:cancelled-component-staging",
            &pool,
            LibraryComponentSourceKind::Library,
        )
        .await
        .expect("retained cancellation source");
        assert_eq!(pool.retained_available_bytes(), Some(0));

        let temp = tempfile::tempdir().expect("cancelled staging root");
        let staging = ManagedDir::open_root(temp.path()).expect("managed cancellation root");
        let slot = ArtifactRelativePath::new("000000/000002").expect("cancellation staging slot");
        let (entered_tx, entered_rx) = oneshot::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let task_staging = staging.clone();
        let task_slot = slot.clone();
        let task = tokio::spawn(async move {
            source
                .stage_create_new_with_hook(
                    &task_staging,
                    &task_slot,
                    Box::new(move || {
                        let _ = entered_tx.send(());
                        let (lock, condition) = &*release_for_hook;
                        let released = lock.lock().expect("staging gate lock");
                        drop(
                            condition
                                .wait_while(released, |released| !*released)
                                .expect("staging gate wait"),
                        );
                    }),
                )
                .await
        });
        entered_rx.await.expect("blocking staging owns source");
        assert_eq!(pool.retained_available_bytes(), Some(0));

        task.abort();
        let _ = task.await;
        assert_eq!(pool.retained_available_bytes(), Some(0));

        let (lock, condition) = &*release;
        *lock.lock().expect("staging release lock") = true;
        condition.notify_one();
        for _ in 0..100 {
            if pool.retained_available_bytes() == Some(body.len() as u64) {
                assert_eq!(
                    std::fs::read(slot.join_under(temp.path()))
                        .expect("completed detached staging copy"),
                    body
                );
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(pool.retained_available_bytes(), Some(body.len() as u64));
    }

    #[tokio::test]
    async fn rejects_sha_mismatch() {
        let body = jar_bytes(b"sha-mismatch");
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some("0000000000000000000000000000000000000000".to_string()),
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;

        let error = rejected(
            acquire(
                &url,
                &expected,
                body.len() as u64,
                "library:sha-mismatch",
                &LibrarySourcePool::new(),
            )
            .await,
            "expected mismatched SHA rejection",
        );

        assert!(error.to_string().contains("checksum does not match"));
    }

    #[tokio::test]
    async fn rejects_size_mismatch() {
        let body = jar_bytes(b"size-mismatch");
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64 + 1),
            sha1: None,
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;

        let error = rejected(
            acquire(
                &url,
                &expected,
                body.len() as u64 + 1,
                "library:size-mismatch",
                &LibrarySourcePool::new(),
            )
            .await,
            "expected mismatched size rejection",
        );

        assert!(error.to_string().contains("size does not match"));
    }

    #[tokio::test]
    async fn rejects_declared_content_length_over_request_cap() {
        let response = ScriptedResponse::partial(65, Vec::new());
        let (url, _) = spawn_server(vec![response]).await;

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                64,
                "library:declared-oversize",
                &LibrarySourcePool::new(),
            )
            .await,
            "expected declared oversize rejection",
        );

        assert!(error.to_string().contains("declared response exceeds"));
    }

    #[tokio::test]
    async fn rejects_stream_over_request_cap_without_content_length() {
        let body = jar_bytes(b"stream-over-cap");
        let cap = body.len() as u64 - 1;
        let (url, _) = spawn_server(vec![ScriptedResponse::without_length(body)]).await;

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                cap,
                "library:stream-oversize",
                &LibrarySourcePool::new(),
            )
            .await,
            "expected streamed oversize rejection",
        );

        assert!(error.to_string().contains("response exceeds"));
    }

    #[tokio::test]
    async fn retries_interrupted_response_with_zero_delays() {
        let body = jar_bytes(b"retry-exact");
        let truncated = body[..body.len() / 2].to_vec();
        let responses = vec![
            ScriptedResponse::partial(body.len(), truncated.clone()),
            ScriptedResponse::partial(body.len(), truncated),
            ScriptedResponse::full(body.clone()),
        ];
        let (url, requests) = spawn_server(responses).await;
        let expected = ExpectedIntegrity {
            size: Some(body.len() as u64),
            sha1: Some(sha1_hex(&body)),
        };
        let pool = LibrarySourcePool::new();

        let client = reqwest::Client::new();
        let relative_path = fixture_relative_path();
        let source = acquire_authenticated_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: body.len() as u64,
                target: "library:retry",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO, Duration::ZERO],
        )
        .await
        .expect("retry interrupted streams");

        assert_source(&source, &body, &expected, "library:retry");
        assert_eq!(requests.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn request_timeout_status_is_not_retried() {
        let body = jar_bytes(b"must-not-be-requested");
        let responses = vec![
            ScriptedResponse::status("408 Request Timeout"),
            ScriptedResponse::full(body),
        ];
        let (url, requests) = spawn_server(responses).await;
        let client = reqwest::Client::new();
        let expected = ExpectedIntegrity::default();
        let relative_path = fixture_relative_path();
        let pool = LibrarySourcePool::new();

        let error = acquire_authenticated_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:408",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO],
        )
        .await
        .err()
        .expect("408 must not retry");

        assert!(error.to_string().contains("provider rejected"));
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn rate_limit_status_is_retried() {
        let body = jar_bytes(b"retry-after-rate-limit");
        let responses = vec![
            ScriptedResponse::status("429 Too Many Requests"),
            ScriptedResponse::full(body.clone()),
        ];
        let (url, requests) = spawn_server(responses).await;
        let client = reqwest::Client::new();
        let expected = ExpectedIntegrity::default();
        let relative_path = fixture_relative_path();
        let pool = LibrarySourcePool::new();

        let source = acquire_authenticated_library_source_with_retry_delays(
            LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:429",
                pool: &pool,
                fact_tx: None,
            },
            &[Duration::ZERO],
        )
        .await
        .expect("429 retry");

        assert_source(&source, &body, &expected, "library:429");
        assert_eq!(requests.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn rejects_invalid_zip() {
        let body = b"not-a-jar".to_vec();
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;

        let error = rejected(
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                body.len() as u64,
                "library:invalid-jar",
                &LibrarySourcePool::new(),
            )
            .await,
            "expected invalid JAR rejection",
        );

        assert!(error.to_string().contains("file operation failed"));
    }

    fn eocd(entries: u16, directory_bytes: u32) -> File {
        let mut bytes = vec![0_u8; ZIP_END_OF_CENTRAL_DIRECTORY_BYTES];
        bytes[..4].copy_from_slice(&[0x50, 0x4b, 0x05, 0x06]);
        bytes[8..10].copy_from_slice(&entries.to_le_bytes());
        bytes[10..12].copy_from_slice(&entries.to_le_bytes());
        bytes[12..16].copy_from_slice(&directory_bytes.to_le_bytes());
        let mut file = tempfile::tempfile().expect("preflight file");
        file.write_all(&bytes).expect("write EOCD");
        file.seek(SeekFrom::Start(0)).expect("rewind EOCD");
        file
    }

    #[test]
    fn preflight_rejects_oversized_entry_count_and_central_directory() {
        let too_many_entries = eocd(MAX_JAR_ENTRIES + 1, 1);
        assert!(preflight_zip_central_directory(&too_many_entries).is_err());

        let oversized_directory = eocd(1, MAX_JAR_CENTRAL_DIRECTORY_BYTES + 1);
        assert!(preflight_zip_central_directory(&oversized_directory).is_err());
    }

    #[test]
    fn preflight_rejects_trailing_bytes_and_fake_terminal_eocd() {
        let mut trailing = jar_bytes(b"trailing");
        trailing.push(0);
        let mut trailing_file = tempfile::tempfile().expect("trailing file");
        trailing_file
            .write_all(&trailing)
            .expect("write trailing fixture");
        assert!(preflight_zip_central_directory(&trailing_file).is_err());

        let mut fake_terminal = jar_bytes(b"fake terminal");
        fake_terminal.extend_from_slice(&[
            0x50, 0x4b, 0x05, 0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        ]);
        let mut fake_file = tempfile::tempfile().expect("fake EOCD file");
        fake_file
            .write_all(&fake_terminal)
            .expect("write fake EOCD fixture");
        assert!(preflight_zip_central_directory(&fake_file).is_err());
    }

    #[test]
    fn jar_validation_rejects_oversized_name_budget() {
        let name = "x".repeat(MAX_JAR_ENTRY_NAME_BYTES + 1);
        let mut writer = ZipWriter::new(std::io::Cursor::new(Vec::new()));
        writer
            .start_file(name, SimpleFileOptions::default())
            .expect("start oversized-name entry");
        writer.write_all(b"payload").expect("write payload");
        let bytes = writer.finish().expect("finish fixture").into_inner();
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error = validate_bounded_jar(&file).expect_err("reject oversized name");

        assert!(error.to_string().contains("entry name is too large"));
    }

    #[test]
    fn jar_validation_rejects_single_entry_zip_bomb_metadata() {
        let mut bytes = jar_with_entries(&[b"tiny"]);
        patch_uncompressed_sizes(&mut bytes, &[(MAX_JAR_ENTRY_UNCOMPRESSED_BYTES + 1) as u32]);
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error = validate_bounded_jar(&file).expect_err("reject per-entry ZIP bomb");

        assert!(error.to_string().contains("entry expands beyond"));
    }

    #[test]
    fn jar_validation_rejects_aggregate_zip_bomb_metadata() {
        const ENTRY_SIZE: u32 = 300 << 20;
        let mut bytes = jar_with_entries(&[b"first", b"second"]);
        patch_uncompressed_sizes(&mut bytes, &[ENTRY_SIZE, ENTRY_SIZE]);
        let mut file = tempfile::tempfile().expect("JAR file");
        file.write_all(&bytes).expect("write JAR fixture");
        file.seek(SeekFrom::Start(0)).expect("rewind JAR fixture");

        let error = validate_bounded_jar(&file).expect_err("reject aggregate ZIP bomb");

        assert!(error.to_string().contains("JAR expands beyond"));
    }

    #[tokio::test]
    async fn dropping_carrier_restores_pool_permits() {
        let body = jar_bytes(b"permit-drop");
        let pool = LibrarySourcePool::new();
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;

        let source = acquire(
            &url,
            &ExpectedIntegrity::default(),
            LIBRARY_SOURCE_MAX_BYTES,
            "library:permit-drop",
            &pool,
        )
        .await
        .expect("acquire full-budget source");
        assert_eq!(pool.available_bytes(), 0);

        drop(source);

        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn consuming_parts_preserve_logical_key_contract_and_budget_owner() {
        let body = jar_bytes(b"consume-parts");
        let expected = ExpectedIntegrity {
            size: None,
            sha1: Some(sha1_hex(&body)),
        };
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();
        let source = acquire(
            &url,
            &expected,
            LIBRARY_SOURCE_MAX_BYTES,
            "library:parts",
            &pool,
        )
        .await
        .expect("acquire source parts");

        let (mut file, relative_path, size, sha1, original, target, provider_url, permit) =
            source.into_parts();

        assert_eq!(relative_path, fixture_relative_path());
        assert_eq!(size, body.len() as u64);
        assert_eq!(sha1, sha1_bytes(&body));
        assert_eq!(original, expected);
        assert_eq!(target, "library:parts");
        assert_eq!(provider_url, url);
        let mut actual = Vec::new();
        file.read_to_end(&mut actual).expect("read source parts");
        assert_eq!(actual, body);
        assert_eq!(pool.available_bytes(), 0);
        drop((file, permit));
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn cancellation_during_body_restores_pool_permits() {
        let body = jar_bytes(b"cancel-body");
        let (started_tx, started_rx) = oneshot::channel();
        let (gate_tx, gate_rx) = oneshot::channel();
        let response = ScriptedResponse::gated(body.clone(), started_tx, gate_rx);
        let (url, _) = spawn_server(vec![response]).await;
        let pool = LibrarySourcePool::new();
        let pool_for_task = pool.clone();
        let task = tokio::spawn(async move {
            acquire(
                &url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:cancel-body",
                &pool_for_task,
            )
            .await
        });
        started_rx.await.expect("request reached body gate");
        assert_eq!(pool.available_bytes(), 0);

        task.abort();
        let _ = task.await;
        let _ = gate_tx.send(());

        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn cancellation_during_blocking_validation_keeps_budget_reserved() {
        let body = jar_bytes(b"cancel-validation");
        let (url, _) = spawn_server(vec![ScriptedResponse::full(body.clone())]).await;
        let pool = LibrarySourcePool::new();
        let pool_for_task = pool.clone();
        let (entered_tx, entered_rx) = oneshot::channel();
        let release = Arc::new((Mutex::new(false), Condvar::new()));
        let release_for_hook = Arc::clone(&release);
        let task = tokio::spawn(async move {
            let client = reqwest::Client::new();
            let expected = ExpectedIntegrity::default();
            let relative_path = fixture_relative_path();
            let request = LibrarySourceRequest {
                client: &client,
                url: &url,
                expected: &expected,
                relative_path: &relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: "library:cancel-validation",
                pool: &pool_for_task,
                fact_tx: None,
            };
            acquire_authenticated_library_source_attempt_inner(
                &request,
                Some(Box::new(move || {
                    let _ = entered_tx.send(());
                    let (lock, condition) = &*release_for_hook;
                    let released = lock.lock().expect("validation gate lock");
                    drop(
                        condition
                            .wait_while(released, |released| !*released)
                            .expect("validation gate wait"),
                    );
                })),
            )
            .await
        });
        entered_rx.await.expect("blocking validation entered");
        assert_eq!(pool.available_bytes(), 0);

        task.abort();
        let _ = task.await;
        assert_eq!(pool.available_bytes(), 0);

        let (lock, condition) = &*release;
        *lock.lock().expect("validation release lock") = true;
        condition.notify_one();
        for _ in 0..100 {
            if pool.available_bytes() == LIBRARY_SOURCE_MAX_BYTES {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }

    #[tokio::test]
    async fn sixteen_unknown_sources_serialize_on_the_aggregate_budget() {
        let body = jar_bytes(b"aggregate-budget");
        let (started_tx, started_rx) = oneshot::channel();
        let (gate_tx, gate_rx) = oneshot::channel();
        let mut responses = vec![ScriptedResponse::gated(body.clone(), started_tx, gate_rx)];
        responses.extend((1..16).map(|_| ScriptedResponse::full(body.clone())));
        let (url, requests) = spawn_server(responses).await;
        let pool = LibrarySourcePool::new();

        let first_url = url.clone();
        let first_pool = pool.clone();
        let first = tokio::spawn(async move {
            let source = acquire(
                &first_url,
                &ExpectedIntegrity::default(),
                LIBRARY_SOURCE_MAX_BYTES,
                "library:aggregate-first",
                &first_pool,
            )
            .await?;
            drop(source);
            Ok::<_, DownloadError>(())
        });
        started_rx.await.expect("first request reached body gate");

        let mut followers = Vec::new();
        for index in 1..16 {
            let follower_url = url.clone();
            let follower_pool = pool.clone();
            followers.push(tokio::spawn(async move {
                let target = format!("library:aggregate-{index}");
                let source = acquire(
                    &follower_url,
                    &ExpectedIntegrity::default(),
                    LIBRARY_SOURCE_MAX_BYTES,
                    &target,
                    &follower_pool,
                )
                .await?;
                drop(source);
                Ok::<_, DownloadError>(())
            }));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 1);

        gate_tx.send(()).expect("release first response");
        first.await.expect("first task").expect("first source");
        for follower in followers {
            follower
                .await
                .expect("follower task")
                .expect("follower source");
        }
        assert_eq!(requests.load(Ordering::SeqCst), 16);
    }

    #[tokio::test]
    async fn weighted_known_sources_cannot_overbook_the_aggregate_budget() {
        const EXPECTED_SIZE: u64 = 33 << 20;
        let body = jar_bytes(b"weighted-budget");
        let mut responses = Vec::new();
        let mut gates = Vec::new();
        for _ in 0..16 {
            let (gate_tx, gate_rx) = oneshot::channel();
            gates.push(gate_tx);
            responses.push(ScriptedResponse {
                status: "200 OK",
                content_length: Some(body.len()),
                body: body.clone(),
                started: None,
                body_gate: Some(gate_rx),
            });
        }
        let (url, requests) = spawn_server(responses).await;
        let pool = LibrarySourcePool::new();
        let mut tasks = Vec::new();
        for index in 0..16 {
            let task_url = url.clone();
            let task_pool = pool.clone();
            tasks.push(tokio::spawn(async move {
                let target = format!("library:weighted-{index}");
                let expected = ExpectedIntegrity {
                    size: Some(EXPECTED_SIZE),
                    sha1: None,
                };
                acquire(
                    &task_url,
                    &expected,
                    LIBRARY_SOURCE_MAX_BYTES,
                    &target,
                    &task_pool,
                )
                .await
            }));
        }

        wait_for_requests(&requests, 15).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(requests.load(Ordering::SeqCst), 15);

        for gate in gates {
            let _ = gate.send(());
        }
        for task in tasks {
            assert!(task.await.expect("weighted task").is_err());
        }
        assert_eq!(requests.load(Ordering::SeqCst), 16);
        assert_eq!(pool.available_bytes(), LIBRARY_SOURCE_MAX_BYTES);
    }
}
