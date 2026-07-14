use super::facts::{
    ExecutionDownloadRequest, emit_execution_download_facts, emit_selected_download_descriptor,
    execution_download_error, execution_download_fact, integrity_mismatch_fact, metadata_facts,
    no_download_fact_fields, selected_artifact_missing_fact, selected_download_target_label,
    size_mismatch_fact,
};
use super::integrity::{
    ExistingArtifactIntegrity, download_size_mismatch, existing_artifact_integrity,
    existing_content_addressed_asset_integrity, is_sha1_hex, verify_download_integrity,
};
use super::library_source::AuthenticatedLibrarySource;
use super::model::{
    ActualIntegrity, DownloadError, DownloadIntegrityError, ExecutionDownloadError,
    ExecutionDownloadFact, ExecutionDownloadFactKind, ExecutionDownloadReport, ExpectedIntegrity,
    MaterializedLibraryIdentity, SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind,
};
use super::path_safety::{
    bounded_download_file_label, filesystem_path, safe_download_target_label,
};
use super::promotion::{
    promotion_backup_path, selected_promotion_temp_path, sweep_stale_promotion_backups,
    sweep_stale_selected_promotion_temps,
};
use super::transfer_failure::{
    finish_execution_error, finish_execution_error_after_temp_discard,
    finish_io_failure_after_temp_discard, record_io_failure_fact_pair,
};
use crate::artifact_path::ArtifactRelativePath;
use crate::paths::libraries_dir;
use futures_util::StreamExt;
use sha1::{Digest as _, Sha1};
use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::fs as async_fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
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

pub(crate) struct AuthenticatedSelectedArtifactSource {
    bytes: Arc<[u8]>,
    observed_size: u64,
    observed_sha1: [u8; 20],
    kind: SelectedDownloadArtifactKind,
    provider_url: String,
    logical_identity: String,
    expected: ExpectedIntegrity,
    target: String,
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

    #[cfg(test)]
    pub(super) fn observed_size(&self) -> u64 {
        self.observed_size
    }

    #[cfg(test)]
    pub(super) fn observed_sha1(&self) -> [u8; 20] {
        self.observed_sha1
    }
}

pub(super) struct PreparedSelectedArtifactInstall {
    destination: PathBuf,
    kind: SelectedDownloadArtifactKind,
    provider_url: String,
    logical_identity: String,
    expected: ExpectedIntegrity,
    target: String,
}

pub(super) struct PreparedLibraryPublication {
    relative_path: ArtifactRelativePath,
    is_native: bool,
    selected: PreparedSelectedArtifactInstall,
    provider_url: String,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
}

impl PreparedLibraryPublication {
    #[cfg(test)]
    pub(super) fn expected(&self) -> &ExpectedIntegrity {
        &self.selected.expected
    }

    #[cfg(test)]
    pub(super) fn target(&self) -> &str {
        &self.selected.target
    }
}

impl PreparedSelectedArtifactInstall {
    #[cfg(test)]
    pub(super) fn target(&self) -> &str {
        &self.target
    }
}

pub(crate) struct MaterializedSelectedArtifactSource {
    source: AuthenticatedSelectedArtifactSource,
}

struct PreparedExactPublication {
    source: RetainedExactSource,
    destination: PathBuf,
    target: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ExactPublicationOutcome {
    AlreadyExact,
    Promoted,
}

struct RetainedExactSource {
    file: std::fs::File,
    observed_size: u64,
    observed_sha1: [u8; 20],
    _lifetime_guard: Box<dyn Send + 'static>,
}

impl RetainedExactSource {
    fn new<G>(
        file: std::fs::File,
        observed_size: u64,
        observed_sha1: [u8; 20],
        lifetime_guard: G,
    ) -> Self
    where
        G: Send + 'static,
    {
        Self {
            file,
            observed_size,
            observed_sha1,
            _lifetime_guard: Box::new(lifetime_guard),
        }
    }
}

impl MaterializedSelectedArtifactSource {
    #[cfg(test)]
    pub(crate) fn bytes(&self) -> &[u8] {
        self.source.bytes()
    }

    pub(super) fn shared_bytes(&self) -> Arc<[u8]> {
        Arc::clone(&self.source.bytes)
    }

    pub(crate) fn into_authenticated_source(self) -> AuthenticatedSelectedArtifactSource {
        self.source
    }
}

#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum SelectedPromotionTestStage {
    TempWritten,
    TempValidated,
    BackupOwned,
    PublishedUnverified,
    PublishedVerified,
}

#[cfg(test)]
type SelectedPromotionTestHook =
    Box<dyn FnMut(SelectedPromotionTestStage, &Path, &Path) + Send + 'static>;

#[cfg(test)]
pub(super) struct SelectedPromotionTestControl {
    pub(super) hook: Option<SelectedPromotionTestHook>,
    pub(super) pause_at: Option<SelectedPromotionTestStage>,
    pub(super) reached: Option<tokio::sync::oneshot::Sender<()>>,
    pub(super) resume: Option<tokio::sync::oneshot::Receiver<()>>,
    pub(super) fail_publish_rename: bool,
}

#[cfg(test)]
impl SelectedPromotionTestControl {
    async fn reach(
        &mut self,
        stage: SelectedPromotionTestStage,
        temp_path: &Path,
        destination: &Path,
    ) {
        if let Some(hook) = self.hook.as_mut() {
            hook(stage, temp_path, destination);
        }
        if self.pause_at == Some(stage) {
            if let Some(reached) = self.reached.take() {
                let _ = reached.send(());
            }
            if let Some(resume) = self.resume.take() {
                let _ = resume.await;
            }
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

pub(super) async fn prepare_selected_artifact_install(
    kind: SelectedDownloadArtifactKind,
    destination: &Path,
    url: &str,
    logical_identity: &str,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<PreparedSelectedArtifactInstall, DownloadError> {
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
    Ok(PreparedSelectedArtifactInstall {
        destination: destination.to_path_buf(),
        kind,
        provider_url: url.to_string(),
        logical_identity: logical_identity.to_string(),
        expected: expected.clone(),
        target: selected_download_target_label(kind, destination),
    })
}

pub(super) async fn prepare_library_publication(
    mc_dir: &Path,
    relative_path: ArtifactRelativePath,
    url: &str,
    expected: &ExpectedIntegrity,
    is_native: bool,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<PreparedLibraryPublication, DownloadError> {
    let destination = relative_path.join_under(&libraries_dir(mc_dir));
    guard_existing_unsupported_selected_artifact(
        SelectedDownloadArtifactKind::Library,
        &destination,
        url,
        expected,
        fact_tx,
        None,
    )
    .await?;
    let selected = PreparedSelectedArtifactInstall {
        target: selected_download_target_label(SelectedDownloadArtifactKind::Library, &destination),
        destination,
        kind: SelectedDownloadArtifactKind::Library,
        provider_url: url.to_string(),
        logical_identity: relative_path.as_str().to_string(),
        expected: expected.clone(),
    };
    Ok(PreparedLibraryPublication {
        relative_path,
        is_native,
        selected,
        provider_url: url.to_string(),
        descriptor_tx: descriptor_tx.cloned(),
    })
}

pub(super) async fn materialize_authenticated_library_source(
    prepared: PreparedLibraryPublication,
    source: AuthenticatedLibrarySource,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<(MaterializedLibraryIdentity, ExactPublicationOutcome), DownloadError> {
    if source.relative_path() != &prepared.relative_path
        || source.expected() != &prepared.selected.expected
        || source.provider_url() != prepared.provider_url
    {
        return Err(DownloadError::Integrity(
            "authenticated library source does not match its prepared publication contract"
                .to_string(),
        ));
    }
    let (
        file,
        source_relative_path,
        observed_size,
        observed_sha1,
        expected,
        _source_target,
        source_provider_url,
        permit,
    ) = source.into_parts();
    if source_relative_path != prepared.relative_path
        || expected != prepared.selected.expected
        || source_provider_url != prepared.provider_url
    {
        return Err(DownloadError::Integrity(
            "authenticated library source changed while consuming its publication contract"
                .to_string(),
        ));
    }
    let exact_expected = ExpectedIntegrity {
        size: Some(observed_size),
        sha1: Some(hex_sha1(&observed_sha1)),
    };
    emit_selected_download_descriptor(
        prepared.descriptor_tx.as_ref(),
        SelectedDownloadArtifactKind::Library,
        &prepared.selected.destination,
        &prepared.provider_url,
        &exact_expected,
    );
    emit_selected_artifact_missing_fact_if_absent(
        fact_tx,
        SelectedDownloadArtifactKind::Library,
        &prepared.selected.destination,
        &exact_expected,
    )
    .await;
    let target = prepared.selected.target;
    let destination = prepared.selected.destination.clone();
    let outcome = publish_authenticated_retained_file(
        RetainedExactSource::new(file, observed_size, observed_sha1, permit),
        prepared.selected.destination,
        target.clone(),
        fact_tx.cloned(),
        #[cfg(test)]
        None,
    )
    .await?;
    let mut facts = metadata_facts(&exact_expected, &target);
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::ArtifactVerified,
        &target,
        no_download_fact_fields(),
    ));
    emit_execution_download_facts(fact_tx, &facts);
    Ok((
        MaterializedLibraryIdentity::new(
            prepared.relative_path,
            destination,
            prepared.is_native,
            prepared.provider_url,
            expected,
            observed_size,
            observed_sha1,
        ),
        outcome,
    ))
}

pub(super) async fn materialize_authenticated_selected_artifact_source(
    prepared: PreparedSelectedArtifactInstall,
    source: AuthenticatedSelectedArtifactSource,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
) -> Result<MaterializedSelectedArtifactSource, DownloadError> {
    materialize_authenticated_selected_artifact_source_inner(
        prepared,
        source,
        fact_tx,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
pub(super) async fn materialize_authenticated_selected_artifact_source_with_control(
    prepared: PreparedSelectedArtifactInstall,
    source: AuthenticatedSelectedArtifactSource,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    control: SelectedPromotionTestControl,
) -> Result<MaterializedSelectedArtifactSource, DownloadError> {
    materialize_authenticated_selected_artifact_source_inner(
        prepared,
        source,
        fact_tx,
        Some(control),
    )
    .await
}

async fn materialize_authenticated_selected_artifact_source_inner(
    prepared: PreparedSelectedArtifactInstall,
    mut source: AuthenticatedSelectedArtifactSource,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    #[cfg(test)] control: Option<SelectedPromotionTestControl>,
) -> Result<MaterializedSelectedArtifactSource, DownloadError> {
    if source.kind != prepared.kind
        || source.provider_url != prepared.provider_url
        || source.logical_identity != prepared.logical_identity
        || source.expected != prepared.expected
    {
        return Err(DownloadError::Integrity(
            "authenticated source does not match its prepared install contract".to_string(),
        ));
    }
    source.target.clone_from(&prepared.target);
    let fact_tx = fact_tx.cloned();
    tokio::spawn(async move {
        materialize_authenticated_selected_artifact_source_owned(
            prepared,
            source,
            fact_tx,
            #[cfg(test)]
            control,
        )
        .await
    })
    .await
    .map_err(|error| {
        DownloadError::FileOperation(io::Error::other(format!(
            "selected artifact materialization task failed: {error}"
        )))
    })?
}

async fn materialize_authenticated_selected_artifact_source_owned(
    prepared: PreparedSelectedArtifactInstall,
    source: AuthenticatedSelectedArtifactSource,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    #[cfg(test)] mut control: Option<SelectedPromotionTestControl>,
) -> Result<MaterializedSelectedArtifactSource, DownloadError> {
    let target = source.target.clone();
    let mut facts = Vec::new();
    if matches!(
        async_fs::symlink_metadata(filesystem_path(&prepared.destination).as_ref()).await,
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink()
    ) {
        let destination = prepared.destination.clone();
        let existing = tokio::task::spawn_blocking(move || open_regular_nofollow(&destination))
            .await
            .map_err(|error| DownloadError::FileOperation(io::Error::other(error.to_string())))??;
        if validate_open_source_at_path(
            &existing,
            &prepared.destination,
            source.observed_size,
            source.observed_sha1,
        )
        .await?
            == OpenSourceValidation::Exact
        {
            facts.extend(metadata_facts(&source.expected, &target));
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ArtifactVerified,
                &target,
                no_download_fact_fields(),
            ));
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            return Ok(MaterializedSelectedArtifactSource { source });
        }
    }
    let bytes = Arc::clone(&source.bytes);
    let expected_size = source.observed_size;
    let expected_sha1 = source.observed_sha1;
    let retained = tokio::task::spawn_blocking(move || {
        use std::io::{Seek as _, SeekFrom, Write as _};

        let mut file = tempfile::tempfile()?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_data()?;
        file.seek(SeekFrom::Start(0))?;
        if !open_file_matches_source(&file, expected_size, &expected_sha1)? {
            return Err(io::Error::other(
                "selected source changed while preparing retained publication",
            ));
        }
        Ok::<_, io::Error>(file)
    })
    .await
    .map_err(|error| DownloadError::FileOperation(io::Error::other(error.to_string())))??;

    publish_authenticated_retained_file(
        RetainedExactSource::new(retained, expected_size, expected_sha1, ()),
        prepared.destination.clone(),
        target.clone(),
        fact_tx.clone(),
        #[cfg(test)]
        control.take(),
    )
    .await?;
    facts.extend(metadata_facts(&source.expected, &target));
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::ArtifactVerified,
        &target,
        no_download_fact_fields(),
    ));
    emit_execution_download_facts(fact_tx.as_ref(), &facts);
    Ok(MaterializedSelectedArtifactSource { source })
}

async fn publish_authenticated_retained_file(
    source: RetainedExactSource,
    destination: PathBuf,
    target: String,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    #[cfg(test)] control: Option<SelectedPromotionTestControl>,
) -> Result<ExactPublicationOutcome, DownloadError> {
    let publication = PreparedExactPublication {
        source,
        destination,
        target,
    };
    tokio::spawn(async move {
        publish_authenticated_retained_file_owned(
            publication,
            fact_tx,
            #[cfg(test)]
            control,
        )
        .await
    })
    .await
    .map_err(|error| {
        DownloadError::FileOperation(io::Error::other(format!(
            "authenticated publication task failed: {error}"
        )))
    })?
}

#[cfg(test)]
pub(super) async fn publish_authenticated_retained_file_for_test(
    source: std::fs::File,
    destination: PathBuf,
    observed_size: u64,
    observed_sha1: [u8; 20],
    target: String,
    control: Option<SelectedPromotionTestControl>,
) -> Result<ExactPublicationOutcome, DownloadError> {
    publish_authenticated_retained_file(
        RetainedExactSource::new(source, observed_size, observed_sha1, ()),
        destination,
        target,
        None,
        control,
    )
    .await
}

async fn publish_authenticated_retained_file_owned(
    publication: PreparedExactPublication,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    #[cfg(test)] mut control: Option<SelectedPromotionTestControl>,
) -> Result<ExactPublicationOutcome, DownloadError> {
    let mut facts = Vec::new();
    let PreparedExactPublication {
        source,
        destination,
        target,
    } = publication;
    let RetainedExactSource {
        file: source,
        observed_size,
        observed_sha1,
        _lifetime_guard: lifetime_guard,
    } = source;
    let source = validate_retained_source(source, observed_size, observed_sha1).await?;
    if matches!(
        async_fs::symlink_metadata(filesystem_path(&destination).as_ref()).await,
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink()
    ) {
        let existing_destination = destination.clone();
        let existing =
            tokio::task::spawn_blocking(move || open_regular_nofollow(&existing_destination))
                .await
                .map_err(|error| {
                    DownloadError::FileOperation(io::Error::other(error.to_string()))
                })??;
        if validate_open_source_at_path(&existing, &destination, observed_size, observed_sha1)
            .await?
            == OpenSourceValidation::Exact
        {
            return Ok(ExactPublicationOutcome::AlreadyExact);
        }
    }
    if let Some(parent) = destination.parent()
        && let Err(error) = async_fs::create_dir_all(filesystem_path(parent).as_ref()).await
    {
        record_io_failure_fact_pair(
            &mut facts,
            &target,
            error.kind(),
            ExecutionDownloadFactKind::TempWriteFailed,
        );
        emit_execution_download_facts(fact_tx.as_ref(), &facts);
        return Err(DownloadError::FileOperation(error));
    }

    let temp_path = selected_promotion_temp_path(&destination);
    sweep_stale_selected_promotion_temps(&destination).await?;
    match async_fs::symlink_metadata(filesystem_path(&temp_path).as_ref()).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::OwnershipRefused,
                &target,
                no_download_fact_fields(),
            ));
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            return Err(DownloadError::Integrity(
                "artifact has an unsettled retained publication obligation".to_string(),
            ));
        }
        Err(error) => return Err(DownloadError::FileOperation(error)),
    }

    let mut input = async_fs::File::from_std(source);
    let mut output = async_fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(filesystem_path(&temp_path).as_ref())
        .await
        .map_err(|error| {
            record_io_failure_fact_pair(
                &mut facts,
                &target,
                error.kind(),
                ExecutionDownloadFactKind::TempWriteFailed,
            );
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            DownloadError::FileOperation(error)
        })?;
    let mut hasher = Sha1::new();
    let mut copied = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = input.read(&mut buffer).await.map_err(|error| {
            record_io_failure_fact_pair(
                &mut facts,
                &target,
                error.kind(),
                ExecutionDownloadFactKind::TempWriteFailed,
            );
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            DownloadError::FileOperation(error)
        })?;
        if read == 0 {
            break;
        }
        copied = copied.saturating_add(read as u64);
        if copied > observed_size {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                &target,
                vec![("algorithm", "sha1")],
            ));
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            return Err(DownloadError::Integrity(
                "retained publication source exceeded its authenticated size".to_string(),
            ));
        }
        hasher.update(&buffer[..read]);
        output.write_all(&buffer[..read]).await.map_err(|error| {
            record_io_failure_fact_pair(
                &mut facts,
                &target,
                error.kind(),
                ExecutionDownloadFactKind::TempWriteFailed,
            );
            emit_execution_download_facts(fact_tx.as_ref(), &facts);
            DownloadError::FileOperation(error)
        })?;
    }
    if let Err(error) = output.flush().await {
        record_io_failure_fact_pair(
            &mut facts,
            &target,
            error.kind(),
            ExecutionDownloadFactKind::TempWriteFailed,
        );
        emit_execution_download_facts(fact_tx.as_ref(), &facts);
        return Err(DownloadError::FileOperation(error));
    }
    if let Err(error) = output.sync_data().await {
        record_io_failure_fact_pair(
            &mut facts,
            &target,
            error.kind(),
            ExecutionDownloadFactKind::TempWriteFailed,
        );
        emit_execution_download_facts(fact_tx.as_ref(), &facts);
        return Err(DownloadError::FileOperation(error));
    }
    let copied_sha1: [u8; 20] = hasher.finalize().into();
    if copied != observed_size || copied_sha1 != observed_sha1 {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::ChecksumMismatch,
            &target,
            vec![("algorithm", "sha1")],
        ));
        emit_execution_download_facts(fact_tx.as_ref(), &facts);
        return Err(DownloadError::Integrity(
            "retained publication copy does not match authenticated source".to_string(),
        ));
    }
    let source = input.into_std().await;
    validate_retained_source(source, observed_size, observed_sha1).await?;

    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        &target,
        vec![("bytes", observed_size.to_string())],
    ));
    let output = output.into_std().await;
    #[cfg(test)]
    if let Some(control) = control.as_mut() {
        control
            .reach(
                SelectedPromotionTestStage::TempWritten,
                &temp_path,
                &destination,
            )
            .await;
    }
    let output = validate_publication_temp(
        output,
        &temp_path,
        observed_size,
        observed_sha1,
        &mut facts,
        fact_tx.as_ref(),
        &target,
    )
    .await?;
    #[cfg(test)]
    if let Some(control) = control.as_mut() {
        control
            .reach(
                SelectedPromotionTestStage::TempValidated,
                &temp_path,
                &destination,
            )
            .await;
    }

    publish_authenticated_temp(
        output,
        &temp_path,
        &destination,
        observed_size,
        observed_sha1,
        #[cfg(test)]
        control.as_mut(),
    )
    .await
    .map_err(|error| {
        facts.push(execution_download_fact(
            ExecutionDownloadFactKind::PromoteFailed,
            &target,
            no_download_fact_fields(),
        ));
        emit_execution_download_facts(fact_tx.as_ref(), &facts);
        DownloadError::FileOperation(error)
    })?;
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::Promoted,
        &target,
        no_download_fact_fields(),
    ));
    emit_execution_download_facts(fact_tx.as_ref(), &facts);
    drop(lifetime_guard);
    Ok(ExactPublicationOutcome::Promoted)
}

async fn validate_retained_source(
    mut source: std::fs::File,
    expected_size: u64,
    expected_sha1: [u8; 20],
) -> Result<std::fs::File, DownloadError> {
    tokio::task::spawn_blocking(move || {
        use std::io::{Seek as _, SeekFrom};

        if !open_file_matches_source(&source, expected_size, &expected_sha1)? {
            return Err(io::Error::other(
                "retained publication source no longer matches authenticated bytes",
            ));
        }
        source.seek(SeekFrom::Start(0))?;
        Ok(source)
    })
    .await
    .map_err(|error| DownloadError::FileOperation(io::Error::other(error.to_string())))?
    .map_err(DownloadError::FileOperation)
}

async fn validate_publication_temp(
    output: std::fs::File,
    temp_path: &Path,
    expected_size: u64,
    expected_sha1: [u8; 20],
    facts: &mut Vec<ExecutionDownloadFact>,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    target: &str,
) -> Result<std::fs::File, DownloadError> {
    let path = temp_path.to_path_buf();
    let validation = tokio::task::spawn_blocking(move || {
        let valid = validate_open_source_sync(&output, &path, expected_size, &expected_sha1);
        (output, valid)
    })
    .await;
    let (output, valid) = match validation {
        Ok((output, Ok(valid))) => (output, valid),
        Ok((_output, Err(error))) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                no_download_fact_fields(),
            ));
            emit_execution_download_facts(fact_tx, facts);
            return Err(DownloadError::FileOperation(error));
        }
        Err(error) => {
            facts.push(execution_download_fact(
                ExecutionDownloadFactKind::TempWriteFailed,
                target,
                no_download_fact_fields(),
            ));
            emit_execution_download_facts(fact_tx, facts);
            return Err(DownloadError::FileOperation(io::Error::other(
                error.to_string(),
            )));
        }
    };
    if valid != OpenSourceValidation::Exact {
        facts.push(match valid {
            OpenSourceValidation::ContentMismatch => execution_download_fact(
                ExecutionDownloadFactKind::ChecksumMismatch,
                target,
                vec![("algorithm", "sha1")],
            ),
            OpenSourceValidation::IdentityMismatch => execution_download_fact(
                ExecutionDownloadFactKind::PromoteFailed,
                target,
                no_download_fact_fields(),
            ),
            OpenSourceValidation::Exact => unreachable!(),
        });
        emit_execution_download_facts(fact_tx, facts);
        return Err(DownloadError::Integrity(
            "publication temp does not match authenticated source".to_string(),
        ));
    }
    Ok(output)
}

async fn publish_authenticated_temp(
    output: std::fs::File,
    temp_path: &Path,
    destination: &Path,
    expected_size: u64,
    expected_sha1: [u8; 20],
    #[cfg(test)] mut control: Option<&mut SelectedPromotionTestControl>,
) -> io::Result<()> {
    sweep_stale_promotion_backups(destination).await?;
    if validate_open_source_at_path(&output, temp_path, expected_size, expected_sha1).await?
        != OpenSourceValidation::Exact
    {
        return Err(io::Error::other(
            "authenticated temp identity changed before promotion",
        ));
    }

    let backup_path = promotion_backup_path(destination);
    let existing = match async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {
            let destination = destination.to_path_buf();
            Some(
                tokio::task::spawn_blocking(move || open_regular_nofollow(&destination))
                    .await
                    .map_err(|error| io::Error::other(error.to_string()))??,
            )
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "authenticated artifact destination identity is unsupported",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    let backup = if let Some(existing) = existing {
        let existing_size = existing.metadata()?.len();
        rename_no_replace(destination, &backup_path).await?;
        if !validate_open_identity_at_path(&existing, &backup_path, existing_size).await? {
            return Err(io::Error::other(
                "authenticated artifact backup identity changed during promotion",
            ));
        }
        #[cfg(test)]
        if let Some(control) = control.as_deref_mut() {
            control
                .reach(
                    SelectedPromotionTestStage::BackupOwned,
                    temp_path,
                    destination,
                )
                .await;
        }
        Some((existing, existing_size))
    } else {
        None
    };

    match validate_open_source_at_path(&output, temp_path, expected_size, expected_sha1).await {
        Ok(OpenSourceValidation::Exact) => {}
        Ok(_) => {
            restore_unpublished_backup(backup.as_ref(), &backup_path, destination).await?;
            return Err(io::Error::other(
                "authenticated temp identity changed at promotion boundary",
            ));
        }
        Err(error) => {
            restore_unpublished_backup(backup.as_ref(), &backup_path, destination).await?;
            return Err(error);
        }
    }
    #[cfg(test)]
    let publish_rename = if control
        .as_deref()
        .is_some_and(|control| control.fail_publish_rename)
    {
        Err(io::Error::other("forced authenticated publication failure"))
    } else {
        rename_no_replace(temp_path, destination).await
    };
    #[cfg(not(test))]
    let publish_rename = rename_no_replace(temp_path, destination).await;
    if let Err(error) = publish_rename {
        restore_unpublished_backup(backup.as_ref(), &backup_path, destination).await?;
        return Err(error);
    }
    #[cfg(test)]
    if let Some(control) = control.as_deref_mut() {
        control
            .reach(
                SelectedPromotionTestStage::PublishedUnverified,
                temp_path,
                destination,
            )
            .await;
    }

    let publication_validation =
        validate_open_source_at_path(&output, destination, expected_size, expected_sha1).await;
    if !matches!(publication_validation, Ok(OpenSourceValidation::Exact)) {
        let rollback = rollback_authenticated_publication(
            &output,
            expected_size,
            destination,
            temp_path,
            backup.as_ref(),
            &backup_path,
        )
        .await;
        rollback?;
        return Err(match publication_validation {
            Ok(_) => io::Error::other("published artifact does not match authenticated source"),
            Err(error) => error,
        });
    }
    #[cfg(test)]
    if let Some(control) = control {
        control
            .reach(
                SelectedPromotionTestStage::PublishedVerified,
                temp_path,
                destination,
            )
            .await;
    }

    // A live-process backup is retained as one bounded cleanup obligation. Its deterministic name
    // makes later replacement fail closed instead of accumulating files; the stale-owner sweep
    // retires it after process restart.
    Ok(())
}

async fn restore_unpublished_backup(
    backup: Option<&(std::fs::File, u64)>,
    backup_path: &Path,
    destination: &Path,
) -> io::Result<()> {
    let Some((backup, size)) = backup else {
        return Ok(());
    };
    match async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Ok(_) => {
            return Err(io::Error::other(
                "destination changed before exact backup restoration",
            ));
        }
        Err(error) => return Err(error),
    }
    rename_no_replace(backup_path, destination).await?;
    if !validate_open_identity_at_path(backup, destination, *size).await? {
        return Err(io::Error::other(
            "restored artifact backup identity does not match",
        ));
    }
    Ok(())
}

async fn rollback_authenticated_publication(
    output: &std::fs::File,
    expected_size: u64,
    destination: &Path,
    rejected_path: &Path,
    backup: Option<&(std::fs::File, u64)>,
    backup_path: &Path,
) -> io::Result<()> {
    match async_fs::symlink_metadata(filesystem_path(destination).as_ref()).await {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            restore_unpublished_backup(backup, backup_path, destination).await?;
            return Ok(());
        }
        Ok(_) => {}
        Err(error) => return Err(error),
    }
    if !validate_open_identity_at_path(output, destination, expected_size).await? {
        return Err(io::Error::other(
            "published pathname changed before exact rollback",
        ));
    }
    rename_no_replace(destination, rejected_path).await?;
    if !validate_open_identity_at_path(output, rejected_path, expected_size).await? {
        return Err(io::Error::other(
            "rejected artifact identity changed during rollback",
        ));
    }
    restore_unpublished_backup(backup, backup_path, destination).await?;
    Ok(())
}

async fn validate_open_source_at_path(
    file: &std::fs::File,
    path: &Path,
    expected_size: u64,
    expected_sha1: [u8; 20],
) -> io::Result<OpenSourceValidation> {
    let file = file.try_clone()?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        validate_open_source_sync(&file, &path, expected_size, &expected_sha1)
    })
    .await
    .map_err(|error| io::Error::other(error.to_string()))?
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum OpenSourceValidation {
    Exact,
    ContentMismatch,
    IdentityMismatch,
}

fn validate_open_source_sync(
    file: &std::fs::File,
    path: &Path,
    expected_size: u64,
    expected_sha1: &[u8; 20],
) -> io::Result<OpenSourceValidation> {
    if !open_file_matches_source(file, expected_size, expected_sha1)? {
        return Ok(OpenSourceValidation::ContentMismatch);
    }
    if !path_matches_open_file(file, path, expected_size)? {
        return Ok(OpenSourceValidation::IdentityMismatch);
    }
    Ok(OpenSourceValidation::Exact)
}

async fn validate_open_identity_at_path(
    file: &std::fs::File,
    path: &Path,
    expected_size: u64,
) -> io::Result<bool> {
    let file = file.try_clone()?;
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || path_matches_open_file(&file, &path, expected_size))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

fn open_file_matches_source(
    file: &std::fs::File,
    expected_size: u64,
    expected_sha1: &[u8; 20],
) -> io::Result<bool> {
    use std::io::{Read as _, Seek as _, SeekFrom};

    if file.metadata()?.len() != expected_size {
        return Ok(false);
    }
    let mut reader = file;
    reader.seek(SeekFrom::Start(0))?;
    let mut hasher = Sha1::new();
    let mut observed = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        observed = observed.saturating_add(read as u64);
        hasher.update(&buffer[..read]);
    }
    let sha1: [u8; 20] = hasher.finalize().into();
    Ok(observed == expected_size && sha1 == *expected_sha1)
}

async fn rename_no_replace(source: &Path, destination: &Path) -> io::Result<()> {
    let source = source.to_path_buf();
    let destination = destination.to_path_buf();
    tokio::task::spawn_blocking(move || rename_no_replace_sync(&source, &destination))
        .await
        .map_err(|error| io::Error::other(error.to_string()))?
}

#[cfg(unix)]
fn rename_no_replace_sync(source: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};

    renameat_with(
        CWD,
        filesystem_path(source).as_ref(),
        CWD,
        filesystem_path(destination).as_ref(),
        RenameFlags::NOREPLACE,
    )
    .map_err(io::Error::from)
}

#[cfg(windows)]
fn rename_no_replace_sync(source: &Path, destination: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows_sys::Win32::Storage::FileSystem::MoveFileExW;

    let source = filesystem_path(source)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let destination = filesystem_path(destination)
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let result = unsafe { MoveFileExW(source.as_ptr(), destination.as_ptr(), 0) };
    if result == 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

#[cfg(unix)]
fn open_regular_nofollow(path: &Path) -> io::Result<std::fs::File> {
    use rustix::fs::{Mode, OFlags};

    let file = std::fs::File::from(
        rustix::fs::open(
            filesystem_path(path).as_ref(),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(io::Error::from)?,
    );
    if !file.metadata()?.is_file() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected artifact destination is not a regular file",
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_regular_nofollow(path: &Path) -> io::Result<std::fs::File> {
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE,
        FILE_SHARE_READ, FILE_SHARE_WRITE,
    };

    let file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(filesystem_path(path).as_ref())?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "selected artifact destination is not a regular file",
        ));
    }
    Ok(file)
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
        target: request.target.to_string(),
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

pub(super) async fn ensure_selected_artifact_with_client(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<Option<ExecutionDownloadReport>, DownloadError> {
    ensure_selected_artifact_with_client_and_observed_size(
        kind,
        client,
        url,
        destination,
        expected,
        fact_tx,
        descriptor_tx,
    )
    .await
    .map(|(report, _)| report)
}

pub(super) async fn ensure_selected_artifact_with_client_and_observed_size(
    kind: SelectedDownloadArtifactKind,
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
    fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
) -> Result<(Option<ExecutionDownloadReport>, u64), DownloadError> {
    match selected_existing_artifact_integrity(kind, destination, expected).await? {
        ExistingArtifactIntegrity::Verified(size) => Ok((None, size)),
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
        .map(|report| {
            let size = report.bytes_written;
            (Some(report), size)
        }),
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
        .map(|report| {
            let size = report.bytes_written;
            (Some(report), size)
        }),
    }
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

pub(super) async fn download_file_with_client_report(
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
    retry_delays: &[Duration],
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    let mut next_delay = 0_usize;
    loop {
        match execute_download_to_temp_inner(
            client,
            ExecutionDownloadRequest::launcher_managed(url, destination, expected),
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

#[cfg(test)]
pub(super) async fn execute_download_to_temp(
    client: &reqwest::Client,
    request: ExecutionDownloadRequest<'_>,
) -> Result<ExecutionDownloadReport, ExecutionDownloadError> {
    execute_download_to_temp_inner(client, request).await
}

async fn execute_download_to_temp_inner(
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
    if request.expected.sha1.is_none() {
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
        .read(true)
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
    let output = output.into_std().await;
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::WrittenToTemp,
        &target,
        vec![("bytes", written.to_string())],
    ));

    let sha1: [u8; 20] = hasher.finalize().into();
    let actual = ActualIntegrity {
        size: written,
        sha1: Some(hex_sha1(&sha1)),
    };
    if let Err(error) = verify_download_integrity(request.destination, request.expected, &actual) {
        drop(output);
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
    facts.push(execution_download_fact(
        ExecutionDownloadFactKind::ArtifactVerified,
        &target,
        no_download_fact_fields(),
    ));
    drop(output);
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

fn hex_sha1(digest: &[u8; 20]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut value = String::with_capacity(40);
    for byte in digest {
        value.push(HEX[(byte >> 4) as usize] as char);
        value.push(HEX[(byte & 0x0f) as usize] as char);
    }
    value
}

#[cfg(unix)]
fn path_matches_open_file(file: &std::fs::File, path: &Path, size: u64) -> io::Result<bool> {
    use rustix::fs::{Mode, OFlags};
    use std::os::unix::fs::MetadataExt as _;

    let held = file.metadata()?;
    let path_file = std::fs::File::from(rustix::fs::open(
        filesystem_path(path).as_ref(),
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let path = path_file.metadata()?;
    Ok(path.is_file()
        && held.len() == size
        && path.len() == size
        && held.dev() == path.dev()
        && held.ino() == path.ino())
}

#[cfg(windows)]
fn path_matches_open_file(file: &std::fs::File, path: &Path, size: u64) -> io::Result<bool> {
    use std::mem::size_of;
    use std::os::windows::fs::{MetadataExt as _, OpenOptionsExt as _};
    use std::os::windows::io::AsRawHandle as _;
    use windows_sys::Win32::Storage::FileSystem::{
        FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_OPEN_REPARSE_POINT, FILE_ID_INFO,
        FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE, FileIdInfo,
        GetFileInformationByHandleEx,
    };

    fn identity(file: &std::fs::File) -> io::Result<(u64, [u8; 16])> {
        let mut value = FILE_ID_INFO::default();
        let size = u32::try_from(size_of::<FILE_ID_INFO>())
            .map_err(|_| io::Error::other("Windows file identity is too large"))?;
        let ok = unsafe {
            GetFileInformationByHandleEx(
                file.as_raw_handle(),
                FileIdInfo,
                (&mut value as *mut FILE_ID_INFO).cast(),
                size,
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((value.VolumeSerialNumber, value.FileId.Identifier))
    }

    let path_file = std::fs::OpenOptions::new()
        .read(true)
        .share_mode(FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT)
        .open(filesystem_path(path).as_ref())?;
    let metadata = path_file.metadata()?;
    if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 || !metadata.is_file() {
        return Ok(false);
    }
    Ok(file.metadata()?.len() == size
        && path_file.metadata()?.len() == size
        && identity(file)? == identity(&path_file)?)
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
