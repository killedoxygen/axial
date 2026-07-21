use super::asset_source::{
    AssetSourcePool, AuthenticatedAssetCacheProofSet, RetainedAssetComponentSource,
    RetainedAssetSourceSet,
};
#[cfg(test)]
use super::assets::AssetDownloadPipeline;
use super::assets::{
    ASSET_OBJECT_BASE_URL, AssetDownloadPipelineEvent, AssetSourceAcquisitionInputs,
    AssetSourceAcquisitionRequest, RetainedAssetsAcquisition, abort_asset_download_pipeline,
    acquire_asset_sources_with_cache, asset_cache_error, next_asset_download_pipeline_event,
    prepare_asset_download_pipeline, spawn_asset_download_pipeline,
};
use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::facts::selected_download_source_label;
use super::libraries::{
    ExactLibraryCacheAdmission, RetainedClassifiedLibraryAcquisition,
    acquire_retained_classified_library, library_jobs_for,
};
use super::library_source::{
    AuthenticatedLibraryCacheProof, AuthenticatedLibraryCacheProofSet,
    AuthenticatedLocalLibraryBytes, LIBRARY_SOURCE_MAX_BYTES, LibraryComponentSourceKind,
    LibrarySourcePool, LibrarySourceRequest, RetainedLibraryComponentSource,
    RetainedLibrarySourceSet, acquire_retained_library_component_source,
};
use super::model::{
    DownloadError, DownloadProgress, ExactLibraryDownloadProof, ExecutionDownloadFact,
    ExpectedIntegrity, SelectedDownloadArtifactKind, progress,
};
use super::plan::{TransferPlan, TransferPlanContribution};
#[cfg(test)]
use super::runtime::spawn_test_runtime_source_pipeline;
use super::runtime::{
    RuntimeEnsurePipelineEvent, next_runtime_pipeline_event, settle_runtime_pipeline_after_failure,
    spawn_runtime_ensure_pipeline,
};
use super::transfer::{
    AuthenticatedSelectedArtifactSource, AuthenticatedSelectedArtifactVersionBundleParts,
    SelectedArtifactSourceRequest, acquire_authenticated_selected_artifact_source,
};
use crate::portable_path::PortableFileName;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodInstallReceipt, KnownGoodIntegrity,
    KnownGoodReconstructionReceipt, KnownGoodRoot, MAX_KNOWN_GOOD_ASSET_INDEX_BYTES,
    MAX_KNOWN_GOOD_VERSION_JSON_BYTES, MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection,
    ManagedKnownGoodComponent, PendingKnownGoodInstallAuthority, RetainedKnownGoodReconstruction,
    authenticate_pending_known_good_install, seal_reconstructed_vanilla,
};
use crate::known_good_libraries::{
    ClassifiedLibraryDownload, LibraryAcquisition, PendingExactLibraryDeclarations,
    PendingStreamedLibraryDeclarations, SealedExactLibraryDeclarations,
    seal_vanilla_exact_library_declarations,
};
use crate::launch::{VersionJson, effective_java_version_for};
use crate::managed_blocking::{ManagedBlockingAttemptGuard, ManagedBlockingWorkers};
use crate::managed_component_cache::ManagedComponentExactCache;
#[cfg(test)]
use crate::managed_component_lifecycle::publish_managed_component_effect_rolling_back_after_first_row_for_test;
use crate::managed_component_lifecycle::{
    ManagedComponentLifecycleOutcome, publish_managed_component_effect,
};
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{
    ManagedRootPublicationLease, open_managed_target_parent, run_publication_blocking,
    validate_existing_managed_target_path,
};
use crate::manifest::{
    ManifestEntry, VersionManifest, fetch_fresh_install_version_manifest,
    fetch_registered_repair_version_manifest,
};
use crate::rules::{Environment, default_environment};
use crate::runtime::{ManagedRuntimeCache, RuntimeSourceReceipt, acquire_preferred_runtime_source};
#[cfg(test)]
use crate::runtime::{
    TestRuntimeSourceDescriptor, acquire_test_runtime_source, authenticated_test_runtime_source,
};
use crate::version_bundle_publication::{
    VersionBundleTransactionSettledOutcome, publish_version_bundle,
    settle_version_bundle_publication,
};
use futures_util::{FutureExt, StreamExt};
use sha1::{Digest as _, Sha1};
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::mpsc;

pub struct Downloader {
    root: DownloaderRoot,
    client: reqwest::Client,
    asset_object_base_url: Arc<str>,
    #[cfg(test)]
    install_manifest: Option<VersionManifest>,
    #[cfg(test)]
    runtime_source: Option<TestRuntimeSourceDescriptor>,
    #[cfg(test)]
    acquisition_workers: Option<ManagedBlockingWorkers>,
    #[cfg(test)]
    wait_for_concurrent_terminals: bool,
}

enum DownloaderRoot {
    Managed {
        library_root: PathBuf,
        runtime_cache: ManagedRuntimeCache,
    },
    SourceOnly,
}

struct SelectedSourceTaskSpec {
    kind: SelectedDownloadArtifactKind,
    url: String,
    logical_identity: String,
    expected: ExpectedIntegrity,
    max_bytes: usize,
    target: String,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    label: &'static str,
}

struct SelectedSourcePipeline {
    task:
        Option<tokio::task::JoinHandle<Result<AuthenticatedSelectedArtifactSource, DownloadError>>>,
    label: &'static str,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReadyConcurrentLane {
    AssetTerminal,
    RuntimeTerminal,
    BufferedLibrarySuccess,
}

fn ready_concurrent_lane(
    asset_terminal: bool,
    runtime_terminal: bool,
    buffered_library_success: bool,
) -> Option<ReadyConcurrentLane> {
    if asset_terminal {
        Some(ReadyConcurrentLane::AssetTerminal)
    } else if runtime_terminal {
        Some(ReadyConcurrentLane::RuntimeTerminal)
    } else if buffered_library_success {
        Some(ReadyConcurrentLane::BufferedLibrarySuccess)
    } else {
        None
    }
}

impl SelectedSourceTaskSpec {
    fn spawn(self, client: reqwest::Client) -> SelectedSourcePipeline {
        let Self {
            kind,
            url,
            logical_identity,
            expected,
            max_bytes,
            target,
            fact_tx,
            label,
        } = self;
        let task = tokio::spawn(async move {
            acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                client: &client,
                kind,
                url: &url,
                logical_identity: &logical_identity,
                expected: &expected,
                max_bytes,
                target: &target,
                fact_tx: fact_tx.as_ref(),
            })
            .await
        });
        SelectedSourcePipeline {
            task: Some(task),
            label,
        }
    }
}

impl SelectedSourcePipeline {
    fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    async fn complete(&mut self) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
        let result = self
            .task
            .as_mut()
            .expect("live selected-source pipeline owns its task")
            .await;
        self.task.take();
        result.map_err(|error| selected_source_task_error(error, self.label))?
    }

    fn abort(&self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }

    async fn drain(&mut self) {
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SelectedSourcePipeline {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub(crate) struct AuthenticatedVersionBundleSource {
    version_id: String,
    members: Vec<AuthenticatedVersionBundleMemberSource>,
}

pub(crate) struct RetainedVersionBundleReconstructionSources {
    version_json: Arc<[u8]>,
    client_jar: Arc<[u8]>,
    log_config: Option<Arc<[u8]>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RegisteredVersionBundleSourceError {
    Source,
    Authority,
    LocalPreparation,
}

#[derive(Clone)]
struct VersionBundleMemberContract {
    kind: KnownGoodArtifactKind,
    root_name: &'static str,
    relative_path: String,
    logical_identity: String,
    digest: String,
    size: u64,
}

#[derive(Clone)]
struct VersionBundleContract {
    version_json: VersionBundleMemberContract,
    client_jar: VersionBundleMemberContract,
    log_config: Option<VersionBundleMemberContract>,
}

#[derive(Default)]
struct ExactLocalVersionBundleSources {
    version_json: Option<AuthenticatedVersionBundleMemberSource>,
    client_jar: Option<AuthenticatedVersionBundleMemberSource>,
    log_config: Option<AuthenticatedVersionBundleMemberSource>,
}

pub(crate) struct AuthenticatedVersionBundleMemberSource {
    kind: KnownGoodArtifactKind,
    logical_identity: String,
    bytes: Arc<[u8]>,
    observed_sha1: String,
    observed_size: u64,
}

impl AuthenticatedVersionBundleSource {
    pub(crate) fn version_id(&self) -> &str {
        &self.version_id
    }

    pub(crate) fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        version_bundle_sources_match_projection(&self.version_id, projection, &self.members)
    }

    pub(crate) fn into_sources(self) -> Vec<AuthenticatedVersionBundleMemberSource> {
        self.members
    }

    fn from_members(
        version_id: String,
        projection: &ManagedComponentProjection<'_>,
        members: Vec<AuthenticatedVersionBundleMemberSource>,
    ) -> Result<Self, DownloadError> {
        let source = Self {
            version_id,
            members,
        };
        if !source.matches_projection(projection) {
            return Err(version_bundle_install_error(
                "VersionBundle sources do not match the admitted projection",
            ));
        }
        Ok(source)
    }

    fn from_selected_sources(
        version_id: String,
        version_json: AuthenticatedSelectedArtifactSource,
        client_jar: AuthenticatedSelectedArtifactSource,
        log_config: Option<AuthenticatedSelectedArtifactSource>,
    ) -> Result<Self, DownloadError> {
        let mut members = Vec::with_capacity(2 + usize::from(log_config.is_some()));
        members.push(AuthenticatedVersionBundleMemberSource::from_selected(
            version_json,
        )?);
        members.push(AuthenticatedVersionBundleMemberSource::from_selected(
            client_jar,
        )?);
        if let Some(log_config) = log_config {
            members.push(AuthenticatedVersionBundleMemberSource::from_selected(
                log_config,
            )?);
        }
        Ok(Self {
            version_id,
            members,
        })
    }

    fn from_local_projection(
        version_id: String,
        projection: &ManagedComponentProjection<'_>,
        version_json: Vec<u8>,
        client_jar: Vec<u8>,
        log_config: Option<Vec<u8>>,
    ) -> Result<Self, DownloadError> {
        let version_json_identity = projected_member_identity(
            &version_id,
            projection,
            KnownGoodArtifactKind::VersionMetadata,
        )?;
        let client_jar_identity =
            projected_member_identity(&version_id, projection, KnownGoodArtifactKind::ClientJar)?;
        let projected_log_identity = projected_optional_log_identity(projection)?;
        if projected_log_identity.is_some() != log_config.is_some() {
            return Err(version_bundle_install_error(
                "local version bundle logging source does not match the admitted projection",
            ));
        }

        let mut members = Vec::with_capacity(2 + usize::from(log_config.is_some()));
        members.push(AuthenticatedVersionBundleMemberSource::from_local(
            KnownGoodArtifactKind::VersionMetadata,
            version_json_identity,
            version_json,
        )?);
        members.push(AuthenticatedVersionBundleMemberSource::from_local(
            KnownGoodArtifactKind::ClientJar,
            client_jar_identity,
            client_jar,
        )?);
        if let (Some(identity), Some(bytes)) = (projected_log_identity, log_config) {
            members.push(AuthenticatedVersionBundleMemberSource::from_local(
                KnownGoodArtifactKind::LogConfig,
                identity,
                bytes,
            )?);
        }
        let source = Self {
            version_id,
            members,
        };
        if !source.matches_projection(projection) {
            return Err(version_bundle_install_error(
                "local version bundle sources do not match the admitted projection",
            ));
        }
        Ok(source)
    }

    pub(crate) fn from_reconstruction_projection(
        version_id: String,
        projection: &ManagedComponentProjection<'_>,
        sources: RetainedVersionBundleReconstructionSources,
    ) -> Result<Self, DownloadError> {
        let version_json_identity = projected_member_identity(
            &version_id,
            projection,
            KnownGoodArtifactKind::VersionMetadata,
        )?;
        let client_jar_identity =
            projected_member_identity(&version_id, projection, KnownGoodArtifactKind::ClientJar)?;
        let projected_log_identity = projected_optional_log_identity(projection)?;
        if projected_log_identity.is_some() != sources.log_config.is_some() {
            return Err(version_bundle_install_error(
                "reconstructed version bundle logging source does not match the projection",
            ));
        }
        let mut members = Vec::with_capacity(2 + usize::from(sources.log_config.is_some()));
        members.push(AuthenticatedVersionBundleMemberSource::from_shared(
            KnownGoodArtifactKind::VersionMetadata,
            version_json_identity,
            sources.version_json,
        )?);
        members.push(AuthenticatedVersionBundleMemberSource::from_shared(
            KnownGoodArtifactKind::ClientJar,
            client_jar_identity,
            sources.client_jar,
        )?);
        if let (Some(identity), Some(bytes)) = (projected_log_identity, sources.log_config) {
            members.push(AuthenticatedVersionBundleMemberSource::from_shared(
                KnownGoodArtifactKind::LogConfig,
                identity,
                bytes,
            )?);
        }
        let source = Self {
            version_id,
            members,
        };
        if !source.matches_projection(projection) {
            return Err(version_bundle_install_error(
                "reconstructed version bundle sources do not match the projection",
            ));
        }
        Ok(source)
    }
}

impl AuthenticatedVersionBundleMemberSource {
    fn from_selected(source: AuthenticatedSelectedArtifactSource) -> Result<Self, DownloadError> {
        let AuthenticatedSelectedArtifactVersionBundleParts {
            bytes,
            observed_size,
            observed_sha1,
            kind: selected_kind,
            logical_identity,
            expected,
        } = source.into_version_bundle_parts();
        let kind = match selected_kind {
            SelectedDownloadArtifactKind::VersionJson => KnownGoodArtifactKind::VersionMetadata,
            SelectedDownloadArtifactKind::ClientJar => KnownGoodArtifactKind::ClientJar,
            SelectedDownloadArtifactKind::LogConfig => KnownGoodArtifactKind::LogConfig,
            _ => {
                return Err(version_bundle_install_error(
                    "selected artifact is not a version bundle member",
                ));
            }
        };
        let observed_sha1 = encode_sha1_digest(observed_sha1);
        if observed_size == 0
            || observed_size > MAX_TIER2_ARTIFACT_BYTES
            || u64::try_from(bytes.len()).ok() != Some(observed_size)
            || expected.size.is_some_and(|size| size != observed_size)
            || !expected
                .sha1
                .as_deref()
                .is_some_and(|digest| digest.eq_ignore_ascii_case(&observed_sha1))
        {
            return Err(version_bundle_install_error(
                "selected version bundle source lacks an exact authenticated contract",
            ));
        }
        Ok(Self {
            kind,
            logical_identity,
            bytes,
            observed_sha1,
            observed_size,
        })
    }

    fn from_local(
        kind: KnownGoodArtifactKind,
        logical_identity: String,
        bytes: Vec<u8>,
    ) -> Result<Self, DownloadError> {
        if !matches!(
            kind,
            KnownGoodArtifactKind::VersionMetadata
                | KnownGoodArtifactKind::ClientJar
                | KnownGoodArtifactKind::LogConfig
        ) || logical_identity.trim().is_empty()
        {
            return Err(version_bundle_install_error(
                "local version bundle member identity is invalid",
            ));
        }
        let observed_size = u64::try_from(bytes.len()).map_err(|_| {
            version_bundle_install_error("local version bundle source is too large")
        })?;
        if observed_size == 0 || observed_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(version_bundle_install_error(
                "local version bundle source exceeds the admitted bounds",
            ));
        }
        let observed_sha1 = format!("{:x}", Sha1::digest(&bytes));
        Ok(Self {
            kind,
            logical_identity,
            bytes: bytes.into(),
            observed_sha1,
            observed_size,
        })
    }

    fn from_shared(
        kind: KnownGoodArtifactKind,
        logical_identity: String,
        bytes: Arc<[u8]>,
    ) -> Result<Self, DownloadError> {
        if !matches!(
            kind,
            KnownGoodArtifactKind::VersionMetadata
                | KnownGoodArtifactKind::ClientJar
                | KnownGoodArtifactKind::LogConfig
        ) || logical_identity.trim().is_empty()
        {
            return Err(version_bundle_install_error(
                "reconstructed version bundle member identity is invalid",
            ));
        }
        let observed_size = u64::try_from(bytes.len()).map_err(|_| {
            version_bundle_install_error("reconstructed version bundle source is too large")
        })?;
        if observed_size == 0 || observed_size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(version_bundle_install_error(
                "reconstructed version bundle source exceeds the admitted bounds",
            ));
        }
        let observed_sha1 = format!("{:x}", Sha1::digest(&bytes));
        Ok(Self {
            kind,
            logical_identity,
            bytes,
            observed_sha1,
            observed_size,
        })
    }

    pub(crate) fn kind(&self) -> KnownGoodArtifactKind {
        self.kind
    }

    pub(crate) fn logical_identity(&self) -> &str {
        &self.logical_identity
    }

    pub(crate) fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    fn observed_sha1(&self) -> &str {
        &self.observed_sha1
    }

    fn observed_size(&self) -> u64 {
        self.observed_size
    }
}

impl RetainedVersionBundleReconstructionSources {
    fn from_authenticated(
        version_json: &AuthenticatedSelectedArtifactSource,
        client_jar: &AuthenticatedSelectedArtifactSource,
        log_config: Option<&AuthenticatedSelectedArtifactSource>,
    ) -> Result<Self, DownloadError> {
        if version_json.kind() != SelectedDownloadArtifactKind::VersionJson
            || client_jar.kind() != SelectedDownloadArtifactKind::ClientJar
            || log_config
                .is_some_and(|source| source.kind() != SelectedDownloadArtifactKind::LogConfig)
        {
            return Err(version_bundle_install_error(
                "reconstructed version bundle selected-source shape is invalid",
            ));
        }
        Ok(Self {
            version_json: version_json.shared_bytes(),
            client_jar: client_jar.shared_bytes(),
            log_config: log_config.map(AuthenticatedSelectedArtifactSource::shared_bytes),
        })
    }

    pub(crate) fn replace_final(self, version_json: Vec<u8>, client_jar: Option<Vec<u8>>) -> Self {
        Self {
            version_json: version_json.into(),
            client_jar: client_jar.map(Arc::<[u8]>::from).unwrap_or(self.client_jar),
            log_config: self.log_config,
        }
    }

    #[cfg(test)]
    pub(crate) fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        if projection.component() != ManagedKnownGoodComponent::VersionBundle {
            return false;
        }
        let expected_count = 2 + usize::from(self.log_config.is_some());
        projection.entry_count() == expected_count
            && projection.entries().iter().all(|projected| {
                let bytes = match projected.entry().kind() {
                    KnownGoodArtifactKind::VersionMetadata => self.version_json.as_ref(),
                    KnownGoodArtifactKind::ClientJar => self.client_jar.as_ref(),
                    KnownGoodArtifactKind::LogConfig => {
                        let Some(bytes) = self.log_config.as_deref() else {
                            return false;
                        };
                        bytes
                    }
                    _ => return false,
                };
                let (KnownGoodIntegrity::Sha1 { digest, size }
                | KnownGoodIntegrity::ExactBytes { digest, size }) = projected.entry().integrity()
                else {
                    return false;
                };
                *size == bytes.len() as u64
                    && digest.as_str() == format!("{:x}", Sha1::digest(bytes))
            })
    }

    #[cfg(any(test, feature = "test-support"))]
    pub(crate) fn from_local_final(
        version_json: Vec<u8>,
        client_jar: Vec<u8>,
        log_config: Option<Vec<u8>>,
    ) -> Self {
        Self {
            version_json: version_json.into(),
            client_jar: client_jar.into(),
            log_config: log_config.map(Arc::<[u8]>::from),
        }
    }
}

fn encode_sha1_digest(digest: [u8; 20]) -> String {
    use std::fmt::Write as _;

    let mut encoded = String::with_capacity(40);
    for byte in digest {
        let _ = write!(encoded, "{byte:02x}");
    }
    encoded
}

pub(crate) struct ReconstructedVanillaClientAuthority {
    reconstruction: RetainedKnownGoodReconstruction,
    client_source: AuthenticatedSelectedArtifactSource,
}

#[derive(Clone)]
pub(crate) struct ManagedReconstructionContext {
    mode: ManagedReconstructionMode,
    workers: ManagedBlockingWorkers,
    blocking_operation: Arc<tokio::sync::Mutex<()>>,
}

struct ManagedReconstructionAttempt {
    operation: Option<tokio::sync::OwnedMutexGuard<()>>,
    workers: ManagedBlockingWorkers,
    runtime: tokio::runtime::Handle,
}

struct ManagedReconstructionDrainMonitor {
    operation: tokio::sync::OwnedMutexGuard<()>,
    workers: ManagedBlockingWorkers,
}

#[derive(Clone)]
enum ManagedReconstructionMode {
    ProofOnly,
    VersionBundle,
    Libraries(ManagedLibrariesReconstructionContext),
    Assets(ManagedAssetsReconstructionContext),
}

#[derive(Clone)]
struct ManagedLibrariesReconstructionContext {
    source_pool: LibrarySourcePool,
    cache_admission: ExactLibraryCacheAdmission,
    cache_proofs: Arc<std::sync::Mutex<AuthenticatedLibraryCacheProofSet>>,
}

#[derive(Clone)]
struct ManagedAssetsReconstructionContext {
    source_pool: AssetSourcePool,
    cache: ManagedComponentExactCache,
    authority:
        Arc<std::sync::Mutex<Option<(RetainedAssetSourceSet, AuthenticatedAssetCacheProofSet)>>>,
}

impl ManagedReconstructionContext {
    pub(crate) fn proof_only() -> Self {
        let workers = ManagedBlockingWorkers::new();
        Self {
            mode: ManagedReconstructionMode::ProofOnly,
            workers,
            blocking_operation: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    pub(crate) async fn bind_libraries(managed_root: ManagedDir) -> Result<Self, DownloadError> {
        let workers = ManagedBlockingWorkers::new();
        let attempt = workers.attempt_guard();
        let result = async {
            Ok(Self {
                mode: ManagedReconstructionMode::Libraries(ManagedLibrariesReconstructionContext {
                    source_pool: LibrarySourcePool::new_with_workers(workers.clone())?,
                    cache_admission: ExactLibraryCacheAdmission::bind_guarded_with_workers(
                        managed_root,
                        workers.clone(),
                    )
                    .await?,
                    cache_proofs: Arc::new(std::sync::Mutex::new(
                        AuthenticatedLibraryCacheProofSet::default(),
                    )),
                }),
                workers: workers.clone(),
                blocking_operation: Arc::new(tokio::sync::Mutex::new(())),
            })
        }
        .await;
        settle_new_managed_blocking_scope(workers, attempt, result).await
    }

    pub(crate) async fn bind_assets(managed_root: ManagedDir) -> Result<Self, DownloadError> {
        let workers = ManagedBlockingWorkers::new();
        let attempt = workers.attempt_guard();
        let result = async {
            Ok(Self {
                mode: ManagedReconstructionMode::Assets(ManagedAssetsReconstructionContext {
                    source_pool: AssetSourcePool::new_with_workers(workers.clone())?,
                    cache: ManagedComponentExactCache::bind_guarded_with_workers(
                        managed_root,
                        ManagedComponentKind::Assets,
                        workers.clone(),
                    )
                    .await
                    .map_err(asset_cache_error)?,
                    authority: Arc::new(std::sync::Mutex::new(None)),
                }),
                workers: workers.clone(),
                blocking_operation: Arc::new(tokio::sync::Mutex::new(())),
            })
        }
        .await;
        settle_new_managed_blocking_scope(workers, attempt, result).await
    }

    pub(crate) fn version_bundle() -> Self {
        let workers = ManagedBlockingWorkers::new();
        Self {
            mode: ManagedReconstructionMode::VersionBundle,
            workers,
            blocking_operation: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    fn retains_version_bundle_sources(&self) -> bool {
        matches!(self.mode, ManagedReconstructionMode::VersionBundle)
    }

    async fn blocking_attempt(&self) -> ManagedReconstructionAttempt {
        let operation = Arc::clone(&self.blocking_operation).lock_owned().await;
        ManagedReconstructionAttempt {
            operation: Some(operation),
            workers: self.workers.clone(),
            runtime: tokio::runtime::Handle::current(),
        }
    }

    async fn settle_blocking_attempt<T>(
        &self,
        mut attempt: ManagedReconstructionAttempt,
        result: Result<T, DownloadError>,
    ) -> Result<T, DownloadError> {
        if result.is_err() {
            attempt.workers.cancel();
        }
        attempt.workers.drain().await;
        drop(attempt.operation.take());
        result
    }

    pub(crate) fn retains_library_sources(&self) -> bool {
        matches!(self.mode, ManagedReconstructionMode::Libraries(_))
    }

    fn phase_library_source_pool(&self) -> Result<LibrarySourcePool, DownloadError> {
        match &self.mode {
            ManagedReconstructionMode::Libraries(libraries) => Ok(libraries.source_pool.clone()),
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => {
                LibrarySourcePool::new_with_workers(self.workers.clone())
            }
        }
    }

    async fn requires_retained_exact_source(
        &self,
        job: &super::libraries::DownloadJob,
    ) -> Result<bool, DownloadError> {
        let libraries = match &self.mode {
            ManagedReconstructionMode::Libraries(libraries) => libraries,
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => return Ok(true),
        };
        if libraries
            .cache_admission
            .requires_retained_source(job)
            .await?
        {
            return Ok(true);
        }
        let expected_size = job.expected.size.ok_or_else(|| {
            DownloadError::Integrity("exact library cache proof lost its size".to_string())
        })?;
        let expected_sha1 = job
            .expected
            .sha1
            .as_deref()
            .and_then(super::libraries::decode_sha1)
            .ok_or_else(|| {
                DownloadError::Integrity("exact library cache proof lost its SHA-1".to_string())
            })?;
        libraries
            .cache_proofs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .insert(AuthenticatedLibraryCacheProof::new(
                job.relative_path.clone(),
                component_source_kind(job.is_native),
                expected_size,
                expected_sha1,
            ))?;
        Ok(false)
    }

    async fn retain_exact_cache_source(
        &self,
        job: &super::libraries::DownloadJob,
        source_pool: &LibrarySourcePool,
        kind: LibraryComponentSourceKind,
    ) -> Result<Option<RetainedLibraryComponentSource>, DownloadError> {
        let libraries = match &self.mode {
            ManagedReconstructionMode::Libraries(libraries) => libraries,
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => return Ok(None),
        };
        libraries
            .cache_admission
            .retain_installer_source(job, source_pool, kind)
            .await
    }

    pub(crate) fn take_library_cache_proofs(&self) -> AuthenticatedLibraryCacheProofSet {
        match &self.mode {
            ManagedReconstructionMode::Libraries(libraries) => std::mem::take(
                &mut *libraries
                    .cache_proofs
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            ),
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => AuthenticatedLibraryCacheProofSet::default(),
        }
    }

    fn assets_acquisition(&self) -> Option<(AssetSourcePool, ManagedComponentExactCache)> {
        match &self.mode {
            ManagedReconstructionMode::Assets(assets) => {
                Some((assets.source_pool.clone(), assets.cache.clone()))
            }
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Libraries(_) => None,
        }
    }

    fn retain_assets_authority(
        &self,
        sources: RetainedAssetSourceSet,
        cache_proofs: AuthenticatedAssetCacheProofSet,
    ) -> Result<(), DownloadError> {
        let assets = match &self.mode {
            ManagedReconstructionMode::Assets(assets) => assets,
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Libraries(_) => {
                return Err(DownloadError::Integrity(
                    "non-Assets reconstruction produced retained Assets authority".to_string(),
                ));
            }
        };
        let mut authority = assets
            .authority
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if authority.is_some() {
            return Err(DownloadError::Integrity(
                "Assets reconstruction produced duplicate retained authority".to_string(),
            ));
        }
        *authority = Some((sources, cache_proofs));
        Ok(())
    }

    pub(crate) fn take_assets_authority(
        self,
    ) -> Result<(RetainedAssetSourceSet, AuthenticatedAssetCacheProofSet), DownloadError> {
        let ManagedReconstructionMode::Assets(assets) = self.mode else {
            return Err(DownloadError::Integrity(
                "Assets authority was requested from a different reconstruction mode".to_string(),
            ));
        };
        take_retained_assets_authority(assets.authority)
    }

    pub(crate) async fn retain_local_sources(
        &self,
        sources: Vec<AuthenticatedLocalLibraryBytes>,
    ) -> Result<RetainedLibrarySourceSet, DownloadError> {
        let attempt = self.blocking_attempt().await;
        let result = self.retain_local_sources_unsettled(sources).await;
        self.settle_blocking_attempt(attempt, result).await
    }

    async fn retain_local_sources_unsettled(
        &self,
        sources: Vec<AuthenticatedLocalLibraryBytes>,
    ) -> Result<RetainedLibrarySourceSet, DownloadError> {
        if sources.is_empty() {
            return Ok(RetainedLibrarySourceSet::new());
        }
        let libraries = match &self.mode {
            ManagedReconstructionMode::Libraries(libraries) => libraries,
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => {
                return Err(DownloadError::Integrity(
                    "non-Libraries reconstruction cannot retain local library bytes".to_string(),
                ));
            }
        };
        let mut retained = RetainedLibrarySourceSet::new();
        for source in sources {
            retained.insert(
                libraries
                    .source_pool
                    .retain_authenticated_local_source(source)
                    .await?,
            )?;
        }
        Ok(retained)
    }
}

impl ManagedReconstructionDrainMonitor {
    async fn drain(self) {
        self.workers.drain().await;
        drop(self.operation);
    }
}

impl Drop for ManagedReconstructionAttempt {
    fn drop(&mut self) {
        let Some(operation) = self.operation.take() else {
            return;
        };
        self.workers.cancel();
        drop(
            self.runtime.spawn(
                ManagedReconstructionDrainMonitor {
                    operation,
                    workers: self.workers.clone(),
                }
                .drain(),
            ),
        );
    }
}

async fn settle_new_managed_blocking_scope<T>(
    workers: ManagedBlockingWorkers,
    attempt: ManagedBlockingAttemptGuard,
    result: Result<T, DownloadError>,
) -> Result<T, DownloadError> {
    if result.is_err() {
        workers.cancel();
    }
    workers.drain().await;
    attempt.disarm();
    result
}

fn take_retained_assets_authority(
    authority: Arc<
        std::sync::Mutex<Option<(RetainedAssetSourceSet, AuthenticatedAssetCacheProofSet)>>,
    >,
) -> Result<(RetainedAssetSourceSet, AuthenticatedAssetCacheProofSet), DownloadError> {
    Arc::try_unwrap(authority)
        .map_err(|_| {
            DownloadError::Integrity(
                "Assets reconstruction authority still has live borrowers".to_string(),
            )
        })?
        .into_inner()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .ok_or_else(|| {
            DownloadError::Integrity(
                "Assets reconstruction did not retain source authority".to_string(),
            )
        })
}

pub(crate) struct ReconstructedVanillaProcessorAuthority {
    pending: PendingReconstructedVanillaProcessorAuthority,
    client_source: AuthenticatedSelectedArtifactSource,
    runtime_source: RuntimeSourceReceipt,
}

pub(crate) struct PendingReconstructedVanillaProcessorAuthority {
    parts: VanillaAuthorityParts,
    library_sources: RetainedLibrarySourceSet,
    version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
}

impl ReconstructedVanillaClientAuthority {
    pub(crate) fn consume_for_overlay(
        self,
    ) -> (
        RetainedKnownGoodReconstruction,
        AuthenticatedSelectedArtifactSource,
    ) {
        (self.reconstruction, self.client_source)
    }
}

impl ReconstructedVanillaProcessorAuthority {
    pub(crate) fn into_parts(
        self,
    ) -> (
        PendingReconstructedVanillaProcessorAuthority,
        AuthenticatedSelectedArtifactSource,
        RuntimeSourceReceipt,
    ) {
        (self.pending, self.client_source, self.runtime_source)
    }
}

impl PendingReconstructedVanillaProcessorAuthority {
    pub(crate) fn version(&self) -> &VersionJson {
        &self.parts.version
    }

    pub(crate) fn complete(
        mut self,
        runtime_source: RuntimeSourceReceipt,
    ) -> Result<RetainedKnownGoodReconstruction, DownloadError> {
        if self.parts.runtime_source.is_some() {
            return Err(DownloadError::ResolveManifest(
                "processor runtime authority was already completed".to_string(),
            ));
        }
        self.parts.runtime_source = Some(runtime_source);
        seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(
            self.parts,
            self.library_sources,
            self.version_bundle_sources,
        ))
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "reconstructed source inventory could not be derived: {error:?}"
            ))
        })
    }
}

struct AuthenticatedVanillaPlan {
    version: VersionJson,
    environment: Environment,
    pending_library_declarations: PendingStreamedLibraryDeclarations,
    library_jobs: Vec<ClassifiedLibraryDownload>,
    version_json_source: AuthenticatedSelectedArtifactSource,
    asset_index_source: Option<AuthenticatedSelectedArtifactSource>,
    runtime_source: Option<RuntimeSourceReceipt>,
}

struct AuthenticatedVanillaVersionSource {
    version: VersionJson,
    environment: Environment,
    version_json_source: AuthenticatedSelectedArtifactSource,
}

struct VanillaAuthorityParts {
    version: VersionJson,
    environment: Environment,
    libraries: crate::known_good_libraries::SealedExactLibraryDeclarations,
    version_source: AuthenticatedSelectedArtifactSource,
    asset_index_source: Option<AuthenticatedSelectedArtifactSource>,
    runtime_source: Option<RuntimeSourceReceipt>,
}

pub(crate) struct ReconstructedVanillaAuthority {
    parts: VanillaAuthorityParts,
    library_sources: RetainedLibrarySourceSet,
    version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
}

pub(crate) struct ReconstructedVanillaAuthorityParts {
    pub(crate) version: VersionJson,
    pub(crate) environment: Environment,
    pub(crate) libraries: crate::known_good_libraries::SealedExactLibraryDeclarations,
    pub(crate) version_source: AuthenticatedSelectedArtifactSource,
    pub(crate) asset_source: Option<AuthenticatedSelectedArtifactSource>,
    pub(crate) runtime_source: Option<RuntimeSourceReceipt>,
    pub(crate) library_sources: RetainedLibrarySourceSet,
    pub(crate) version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
}

pub(crate) struct PreparedManagedInstall {
    authority: PendingKnownGoodInstallAuthority,
    version_bundle_source: AuthenticatedVersionBundleSource,
    asset_sources: RetainedAssetSourceSet,
    library_sources: Vec<RetainedLibraryComponentSource>,
}

#[cfg(test)]
impl PreparedManagedInstall {
    pub(crate) fn retained_asset_source_count(&self) -> usize {
        self.asset_sources.len()
    }

    pub(crate) fn retained_library_source_count(&self) -> usize {
        self.library_sources.len()
    }
}

pub(crate) struct AuthenticatedVanillaInstallSources {
    parts: VanillaAuthorityParts,
    client_source: AuthenticatedSelectedArtifactSource,
    log_config_source: Option<AuthenticatedSelectedArtifactSource>,
}

impl ReconstructedVanillaAuthority {
    fn new(
        parts: VanillaAuthorityParts,
        library_sources: RetainedLibrarySourceSet,
        version_bundle_sources: Option<RetainedVersionBundleReconstructionSources>,
    ) -> Self {
        Self {
            parts,
            library_sources,
            version_bundle_sources,
        }
    }

    pub(crate) fn into_parts(self) -> ReconstructedVanillaAuthorityParts {
        let (version, environment, libraries, version_source, asset_index_source, runtime_source) =
            self.parts.into_parts();
        ReconstructedVanillaAuthorityParts {
            version,
            environment,
            libraries,
            version_source,
            asset_source: asset_index_source,
            runtime_source,
            library_sources: self.library_sources,
            version_bundle_sources: self.version_bundle_sources,
        }
    }
}

impl AuthenticatedVanillaInstallSources {
    fn new(
        parts: VanillaAuthorityParts,
        client_source: AuthenticatedSelectedArtifactSource,
        log_config_source: Option<AuthenticatedSelectedArtifactSource>,
    ) -> Self {
        Self {
            parts,
            client_source,
            log_config_source,
        }
    }

    pub(crate) fn authentication_parts(
        &self,
    ) -> (
        &VersionJson,
        &Environment,
        &crate::known_good_libraries::SealedExactLibraryDeclarations,
        &AuthenticatedSelectedArtifactSource,
        Option<&AuthenticatedSelectedArtifactSource>,
        Option<&RuntimeSourceReceipt>,
    ) {
        (
            &self.parts.version,
            &self.parts.environment,
            &self.parts.libraries,
            &self.parts.version_source,
            self.parts.asset_index_source.as_ref(),
            self.parts.runtime_source.as_ref(),
        )
    }

    fn into_version_bundle_source(self) -> Result<AuthenticatedVersionBundleSource, DownloadError> {
        AuthenticatedVersionBundleSource::from_selected_sources(
            self.parts.version.id,
            self.parts.version_source,
            self.client_source,
            self.log_config_source,
        )
    }
}

impl VanillaAuthorityParts {
    fn into_parts(
        self,
    ) -> (
        VersionJson,
        Environment,
        crate::known_good_libraries::SealedExactLibraryDeclarations,
        AuthenticatedSelectedArtifactSource,
        Option<AuthenticatedSelectedArtifactSource>,
        Option<RuntimeSourceReceipt>,
    ) {
        (
            self.version,
            self.environment,
            self.libraries,
            self.version_source,
            self.asset_index_source,
            self.runtime_source,
        )
    }
}

impl Downloader {
    pub fn new(mc_dir: impl Into<PathBuf>, runtime_cache: ManagedRuntimeCache) -> Self {
        Self {
            root: DownloaderRoot::Managed {
                library_root: mc_dir.into(),
                runtime_cache,
            },
            client: standard_minecraft_download_client(),
            asset_object_base_url: Arc::from(ASSET_OBJECT_BASE_URL),
            #[cfg(test)]
            install_manifest: None,
            #[cfg(test)]
            runtime_source: None,
            #[cfg(test)]
            acquisition_workers: None,
            #[cfg(test)]
            wait_for_concurrent_terminals: false,
        }
    }

    pub(crate) fn source_only() -> Self {
        Self {
            root: DownloaderRoot::SourceOnly,
            client: standard_minecraft_download_client(),
            asset_object_base_url: Arc::from(ASSET_OBJECT_BASE_URL),
            #[cfg(test)]
            install_manifest: None,
            #[cfg(test)]
            runtime_source: None,
            #[cfg(test)]
            acquisition_workers: None,
            #[cfg(test)]
            wait_for_concurrent_terminals: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_install_manifest(
        mc_dir: impl Into<PathBuf>,
        manifest: VersionManifest,
    ) -> Self {
        Self {
            root: DownloaderRoot::Managed {
                library_root: mc_dir.into(),
                runtime_cache: ManagedRuntimeCache::isolated_for_test()
                    .expect("isolated downloader runtime cache"),
            },
            client: standard_minecraft_download_client(),
            asset_object_base_url: Arc::from(ASSET_OBJECT_BASE_URL),
            install_manifest: Some(manifest),
            runtime_source: None,
            acquisition_workers: None,
            wait_for_concurrent_terminals: false,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_runtime_source(
        mut self,
        descriptor: TestRuntimeSourceDescriptor,
    ) -> Self {
        self.runtime_source = Some(descriptor);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_asset_object_base_url(mut self, base_url: impl Into<Arc<str>>) -> Self {
        self.asset_object_base_url = base_url.into();
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_acquisition_workers(mut self, workers: ManagedBlockingWorkers) -> Self {
        self.acquisition_workers = Some(workers);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_test_concurrent_terminal_wait(mut self) -> Self {
        self.wait_for_concurrent_terminals = true;
        self
    }

    fn managed_root(&self) -> &Path {
        let DownloaderRoot::Managed { library_root, .. } = &self.root else {
            unreachable!("source-only downloader cannot materialize an installation");
        };
        library_root
    }

    fn managed_runtime_cache(&self) -> &ManagedRuntimeCache {
        let DownloaderRoot::Managed { runtime_cache, .. } = &self.root else {
            unreachable!("source-only downloader cannot materialize a runtime");
        };
        runtime_cache
    }

    pub async fn install_version<F>(
        &self,
        version_id: &str,
        mut send: F,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        Box::pin(self.install_version_with_fact_sender(version_id, &mut send, None)).await
    }

    pub(crate) async fn reconstruct_version(
        &self,
        version_id: &str,
    ) -> Result<KnownGoodReconstructionReceipt, DownloadError> {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        let context = ManagedReconstructionContext::proof_only();
        self.reconstruct_version_inner(version_id, version_manifest_entry, &context)
            .await
            .map(RetainedKnownGoodReconstruction::discard_sources)
    }

    pub(crate) async fn reconstruct_version_authority(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<RetainedKnownGoodReconstruction, DownloadError> {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        self.reconstruct_version_inner(version_id, version_manifest_entry, context)
            .await
    }

    pub(crate) async fn reconstruct_registered_version_bundle_source(
        &self,
        managed_root: ManagedDir,
        version_id: &str,
        inventory: &crate::known_good::KnownGoodInventory,
    ) -> Result<AuthenticatedVersionBundleSource, RegisteredVersionBundleSourceError> {
        validate_install_version_id(version_id)
            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
        let contract = version_bundle_contract(version_id, &projection)
            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
        let local = retain_exact_local_version_bundle_sources(managed_root.clone(), &contract)
            .await
            .map_err(|_| RegisteredVersionBundleSourceError::LocalPreparation)?;

        let (version, version_json_source) = match local.version_json {
            Some(source) => {
                let version = parse_vanilla_version_source(source.bytes(), version_id)
                    .and_then(|version| {
                        validate_vanilla_version_bundle_contracts(&version)?;
                        Ok(version)
                    })
                    .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
                (version, source)
            }
            None => {
                let manifest = fetch_registered_repair_version_manifest(
                    version_id,
                    &contract.version_json.digest,
                )
                .await
                .map_err(|_| RegisteredVersionBundleSourceError::Source)?;
                let entry = registered_version_metadata_manifest_entry(
                    manifest,
                    version_id,
                    &contract.version_json,
                )?;
                let expected = expected_integrity(&contract.version_json);
                let target = selected_download_source_label(
                    SelectedDownloadArtifactKind::VersionJson,
                    version_id,
                );
                let selected =
                    acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                        client: &self.client,
                        kind: SelectedDownloadArtifactKind::VersionJson,
                        url: &entry.url,
                        logical_identity: version_id,
                        expected: &expected,
                        max_bytes: usize::try_from(contract.version_json.size)
                            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?,
                        target: &target,
                        fact_tx: None,
                    })
                    .await
                    .map_err(|_| RegisteredVersionBundleSourceError::Source)?;
                let version = parse_vanilla_version_source(selected.bytes(), version_id)
                    .and_then(|version| {
                        validate_vanilla_version_bundle_contracts(&version)?;
                        Ok(version)
                    })
                    .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
                let source = AuthenticatedVersionBundleMemberSource::from_selected(selected)
                    .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
                (version, source)
            }
        };

        validate_version_bundle_contract_against_metadata(&contract, &version)
            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?;
        let client_jar_source = match local.client_jar {
            Some(source) => source,
            None => {
                let client = version
                    .downloads
                    .client
                    .as_ref()
                    .ok_or(RegisteredVersionBundleSourceError::Authority)?;
                let expected = expected_integrity(&contract.client_jar);
                let target = selected_download_source_label(
                    SelectedDownloadArtifactKind::ClientJar,
                    version_id,
                );
                let selected =
                    acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                        client: &self.client,
                        kind: SelectedDownloadArtifactKind::ClientJar,
                        url: &client.url,
                        logical_identity: version_id,
                        expected: &expected,
                        max_bytes: usize::try_from(contract.client_jar.size)
                            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?,
                        target: &target,
                        fact_tx: None,
                    })
                    .await
                    .map_err(|_| RegisteredVersionBundleSourceError::Source)?;
                AuthenticatedVersionBundleMemberSource::from_selected(selected)
                    .map_err(|_| RegisteredVersionBundleSourceError::Authority)?
            }
        };

        let log_config_source = match (&contract.log_config, local.log_config) {
            (None, None) => None,
            (Some(_), Some(source)) => Some(source),
            (Some(contract), None) => {
                let file = &version
                    .logging
                    .as_ref()
                    .and_then(|logging| logging.client.as_ref())
                    .ok_or(RegisteredVersionBundleSourceError::Authority)?
                    .file;
                let expected = expected_integrity(contract);
                let target = selected_download_source_label(
                    SelectedDownloadArtifactKind::LogConfig,
                    &contract.logical_identity,
                );
                let selected =
                    acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                        client: &self.client,
                        kind: SelectedDownloadArtifactKind::LogConfig,
                        url: &file.url,
                        logical_identity: &contract.logical_identity,
                        expected: &expected,
                        max_bytes: usize::try_from(contract.size)
                            .map_err(|_| RegisteredVersionBundleSourceError::Authority)?,
                        target: &target,
                        fact_tx: None,
                    })
                    .await
                    .map_err(|_| RegisteredVersionBundleSourceError::Source)?;
                Some(
                    AuthenticatedVersionBundleMemberSource::from_selected(selected)
                        .map_err(|_| RegisteredVersionBundleSourceError::Authority)?,
                )
            }
            (None, Some(_)) => return Err(RegisteredVersionBundleSourceError::Authority),
        };

        let mut members = Vec::with_capacity(2 + usize::from(log_config_source.is_some()));
        members.push(version_json_source);
        members.push(client_jar_source);
        members.extend(log_config_source);
        AuthenticatedVersionBundleSource::from_members(version_id.to_string(), &projection, members)
            .map_err(|_| RegisteredVersionBundleSourceError::Authority)
    }

    async fn reconstruct_version_inner(
        &self,
        version_id: &str,
        version_manifest_entry: ManifestEntry,
        context: &ManagedReconstructionContext,
    ) -> Result<RetainedKnownGoodReconstruction, DownloadError> {
        let attempt = context.blocking_attempt().await;
        let result = self
            .reconstruct_version_inner_unsettled(version_id, version_manifest_entry, context)
            .await;
        context.settle_blocking_attempt(attempt, result).await
    }

    async fn reconstruct_version_inner_unsettled(
        &self,
        version_id: &str,
        version_manifest_entry: ManifestEntry,
        context: &ManagedReconstructionContext,
    ) -> Result<RetainedKnownGoodReconstruction, DownloadError> {
        let (authority, library_sources) = self
            .reconstruct_vanilla_authority(version_id, &version_manifest_entry, context)
            .await?;
        let version_bundle_sources = if context.retains_version_bundle_sources() {
            let client_source = self
                .acquire_version_bundle_client_source(version_id, &authority.version)
                .await?;
            let log_config_source = self
                .acquire_version_bundle_log_source(&authority.version)
                .await?;
            Some(
                RetainedVersionBundleReconstructionSources::from_authenticated(
                    &authority.version_source,
                    &client_source,
                    log_config_source.as_ref(),
                )?,
            )
        } else {
            None
        };
        seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(
            authority,
            library_sources,
            version_bundle_sources,
        ))
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "reconstructed source inventory could not be derived: {error:?}"
            ))
        })
    }

    pub(crate) async fn reconstruct_version_with_client_source(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<ReconstructedVanillaClientAuthority, DownloadError> {
        let attempt = context.blocking_attempt().await;
        let result = self
            .reconstruct_version_with_client_source_unsettled(version_id, context)
            .await;
        context.settle_blocking_attempt(attempt, result).await
    }

    async fn reconstruct_version_with_client_source_unsettled(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<ReconstructedVanillaClientAuthority, DownloadError> {
        let (authority, library_sources, client_source, version_bundle_sources) = self
            .reconstruct_version_with_client_authority(version_id, context)
            .await?;
        let reconstruction = seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(
            authority,
            library_sources,
            version_bundle_sources,
        ))
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "reconstructed source inventory could not be derived: {error:?}"
            ))
        })?;
        Ok(ReconstructedVanillaClientAuthority {
            reconstruction,
            client_source,
        })
    }

    pub(crate) async fn reconstruct_version_for_processor(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<ReconstructedVanillaProcessorAuthority, DownloadError> {
        let attempt = context.blocking_attempt().await;
        let result = self
            .reconstruct_version_for_processor_unsettled(version_id, context)
            .await;
        context.settle_blocking_attempt(attempt, result).await
    }

    async fn reconstruct_version_for_processor_unsettled(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<ReconstructedVanillaProcessorAuthority, DownloadError> {
        let (mut authority, library_sources, client_source, version_bundle_sources) = self
            .reconstruct_version_with_client_authority(version_id, context)
            .await?;
        let runtime_source = authority.runtime_source.take().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated base version has no processor runtime source".to_string(),
            )
        })?;
        Ok(ReconstructedVanillaProcessorAuthority {
            pending: PendingReconstructedVanillaProcessorAuthority {
                parts: authority,
                library_sources,
                version_bundle_sources,
            },
            client_source,
            runtime_source,
        })
    }

    async fn acquire_version_bundle_client_source(
        &self,
        version_id: &str,
        version: &VersionJson,
    ) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
        let client = version.downloads.client.as_ref().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated version has no exact client artifact".to_string(),
            )
        })?;
        let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
        let max_bytes = exact_version_bundle_source_limit(client.size, "client")?;
        let target =
            selected_download_source_label(SelectedDownloadArtifactKind::ClientJar, version_id);
        acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
            client: &self.client,
            kind: SelectedDownloadArtifactKind::ClientJar,
            url: &client.url,
            logical_identity: version_id,
            expected: &expected,
            max_bytes,
            target: &target,
            fact_tx: None,
        })
        .await
    }

    async fn acquire_version_bundle_log_source(
        &self,
        version: &VersionJson,
    ) -> Result<Option<AuthenticatedSelectedArtifactSource>, DownloadError> {
        let Some(logging) = version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
        else {
            return Ok(None);
        };
        let expected = ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1);
        let max_bytes = exact_version_bundle_source_limit(logging.file.size, "log config")?;
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::LogConfig,
            &logging.file.id,
        );
        acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
            client: &self.client,
            kind: SelectedDownloadArtifactKind::LogConfig,
            url: &logging.file.url,
            logical_identity: &logging.file.id,
            expected: &expected,
            max_bytes,
            target: &target,
            fact_tx: None,
        })
        .await
        .map(Some)
    }

    async fn reconstruct_version_with_client_authority(
        &self,
        version_id: &str,
        context: &ManagedReconstructionContext,
    ) -> Result<
        (
            VanillaAuthorityParts,
            RetainedLibrarySourceSet,
            AuthenticatedSelectedArtifactSource,
            Option<RetainedVersionBundleReconstructionSources>,
        ),
        DownloadError,
    > {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        let (authority, library_sources) = self
            .reconstruct_vanilla_authority(version_id, &version_manifest_entry, context)
            .await?;
        let client_source = self
            .acquire_version_bundle_client_source(version_id, &authority.version)
            .await?;
        let version_bundle_sources = if context.retains_version_bundle_sources() {
            let log_config_source = self
                .acquire_version_bundle_log_source(&authority.version)
                .await?;
            Some(
                RetainedVersionBundleReconstructionSources::from_authenticated(
                    &authority.version_source,
                    &client_source,
                    log_config_source.as_ref(),
                )?,
            )
        } else {
            None
        };
        Ok((
            authority,
            library_sources,
            client_source,
            version_bundle_sources,
        ))
    }

    async fn reconstruct_vanilla_authority(
        &self,
        version_id: &str,
        version_manifest_entry: &ManifestEntry,
        context: &ManagedReconstructionContext,
    ) -> Result<(VanillaAuthorityParts, RetainedLibrarySourceSet), DownloadError> {
        let AuthenticatedVanillaPlan {
            version,
            environment,
            pending_library_declarations,
            library_jobs,
            version_json_source,
            mut asset_index_source,
            runtime_source,
        } = self
            .acquire_vanilla_plan(version_id, version_manifest_entry, None)
            .await?;
        if let Some((source_pool, cache)) = context.assets_acquisition() {
            let (sources, cache_proofs) = match asset_index_source.take() {
                Some(source) => {
                    let plan = TransferPlan::shared();
                    let contribution = plan.reserve_contribution();
                    let RetainedAssetsAcquisition {
                        asset_index_source: retained_index,
                        sources,
                        cache_proofs,
                    } = acquire_asset_sources_with_cache(
                        AssetSourceAcquisitionRequest::new(
                            AssetSourceAcquisitionInputs {
                                client: self.client.clone(),
                                asset_object_base_url: Arc::clone(&self.asset_object_base_url),
                                asset_index_id: version.asset_index.id.clone(),
                                asset_index_source: source,
                                fact_tx: None,
                                plan: &plan,
                                contribution,
                            },
                            |_| {},
                        )
                        .bind(source_pool, cache),
                    )
                    .await?;
                    asset_index_source = Some(retained_index);
                    (sources, cache_proofs)
                }
                None => (
                    RetainedAssetSourceSet::new(),
                    AuthenticatedAssetCacheProofSet::default(),
                ),
            };
            context.retain_assets_authority(sources, cache_proofs)?;
        }
        let mut library_proofs = Vec::new();
        let mut library_sources = RetainedLibrarySourceSet::new();
        let source_pool = context.phase_library_source_pool()?;
        for classified in library_jobs {
            let (job, acquisition) = classified.into_parts();
            if acquisition == LibraryAcquisition::ExactDeclaration
                && (!context.retains_library_sources()
                    || !context.requires_retained_exact_source(&job).await?)
            {
                continue;
            }
            let target = selected_download_source_label(
                SelectedDownloadArtifactKind::Library,
                job.relative_path.as_str(),
            );
            let source = acquire_retained_library_component_source(
                LibrarySourceRequest {
                    client: &self.client,
                    url: &job.url,
                    expected: &job.expected,
                    relative_path: &job.relative_path,
                    max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                    target: &target,
                    pool: &source_pool,
                    fact_tx: None,
                },
                component_source_kind(job.is_native),
            )
            .await?;
            if acquisition == LibraryAcquisition::FreshStream {
                library_proofs.push(reconstruction_download_proof(&source)?);
            }
            if context.retains_library_sources() {
                library_sources.insert(source)?;
            }
        }
        let library_declarations = pending_library_declarations
            .seal_streamed(library_proofs)
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "reconstructed library declarations could not be completed: {error:?}"
                ))
            })?;

        Ok((
            VanillaAuthorityParts {
                version,
                environment,
                libraries: library_declarations,
                version_source: version_json_source,
                asset_index_source,
                runtime_source,
            },
            library_sources,
        ))
    }

    pub async fn install_version_with_facts<F, G>(
        &self,
        version_id: &str,
        mut send: F,
        mut send_fact: G,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
        G: FnMut(ExecutionDownloadFact),
    {
        let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
        let result =
            Box::pin(self.install_version_with_fact_sender(version_id, &mut send, Some(fact_tx)))
                .await;
        while let Ok(fact) = fact_rx.try_recv() {
            send_fact(fact);
        }
        result
    }

    async fn install_version_with_fact_sender<F>(
        &self,
        version_id: &str,
        send: &mut F,
        fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let managed_root = self.managed_root().to_path_buf();
        let plan = TransferPlan::shared();
        let mut send = |mut progress: DownloadProgress| {
            plan.stamp(&mut progress);
            send(progress)
        };

        let install_result = async {
            validate_install_version_id(version_id)?;
            send(progress(
                "version_json",
                0,
                1,
                Some(format!("{version_id}.json")),
            ));
            let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
            let authenticated = self
                .acquire_vanilla_plan(version_id, &version_manifest_entry, fact_tx.as_ref())
                .await?;
            send(progress(
                "version_json",
                1,
                1,
                Some(format!("{version_id}.json")),
            ));
            let prepared = self
                .install_version_inner(
                    version_id,
                    authenticated,
                    &mut send,
                    &plan,
                    fact_tx.as_ref(),
                )
                .await?;
            publish_prepared_managed_install(managed_root, prepared).await
        }
        .await;

        match install_result {
            Ok(receipt) => {
                send(DownloadProgress {
                    phase: "done".to_string(),
                    current: 1,
                    total: 1,
                    file: None,
                    error: None,
                    done: true,
                    bytes_done: None,
                    bytes_total: None,
                });
                Ok(receipt)
            }
            Err(error) => {
                send(DownloadProgress {
                    phase: "error".to_string(),
                    current: 0,
                    total: 0,
                    file: None,
                    error: Some(error.to_string()),
                    done: true,
                    bytes_done: None,
                    bytes_total: None,
                });
                Err(error)
            }
        }
    }

    async fn install_version_inner<F>(
        &self,
        version_id: &str,
        authenticated: AuthenticatedVanillaPlan,
        send: &mut F,
        plan: &Arc<TransferPlan>,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    ) -> Result<PreparedManagedInstall, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let AuthenticatedVanillaPlan {
            version,
            environment,
            pending_library_declarations,
            library_jobs,
            version_json_source,
            asset_index_source,
            runtime_source,
        } = authenticated;
        let version_json_bytes = version_json_source.observed_size();
        plan.contribute_total(version_json_bytes);
        plan.add_done(version_json_bytes);
        let asset_index_bytes = asset_index_source
            .as_ref()
            .map(AuthenticatedSelectedArtifactSource::observed_size)
            .unwrap_or(0);
        plan.contribute_total(asset_index_bytes);
        let client_jar_bytes = version
            .downloads
            .client
            .as_ref()
            .and_then(|client| ExpectedIntegrity::from_mojang(client.size, &client.sha1).size)
            .unwrap_or(0);
        plan.contribute_total(client_jar_bytes);
        let log_config_bytes = version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            .and_then(|client| {
                ExpectedIntegrity::from_mojang(client.file.size, &client.file.sha1).size
            })
            .unwrap_or(0);
        plan.contribute_total(log_config_bytes);
        let library_contributions = library_jobs
            .iter()
            .map(|classified| match classified.job().expected.size {
                Some(bytes) => {
                    plan.contribute_total(bytes);
                    None
                }
                None => Some(plan.reserve_contribution()),
            })
            .collect::<Vec<_>>();
        let asset_contribution = asset_index_source
            .as_ref()
            .map(|_| plan.reserve_contribution());
        let runtime_contribution = runtime_source.as_ref().map(|_| plan.reserve_contribution());

        let client = version.downloads.client.as_ref().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated version has no exact client artifact".to_string(),
            )
        })?;
        let client_source_spec = SelectedSourceTaskSpec {
            kind: SelectedDownloadArtifactKind::ClientJar,
            url: client.url.clone(),
            logical_identity: version_id.to_string(),
            expected: ExpectedIntegrity::from_mojang(client.size, &client.sha1),
            max_bytes: exact_version_bundle_source_limit(client.size, "client")?,
            target: selected_download_source_label(
                SelectedDownloadArtifactKind::ClientJar,
                version_id,
            ),
            fact_tx: fact_tx.cloned(),
            label: "client",
        };
        let log_config_source_spec = version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
            .map(|logging| {
                Ok::<_, DownloadError>(SelectedSourceTaskSpec {
                    kind: SelectedDownloadArtifactKind::LogConfig,
                    url: logging.file.url.clone(),
                    logical_identity: logging.file.id.clone(),
                    expected: ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1),
                    max_bytes: exact_version_bundle_source_limit(logging.file.size, "log config")?,
                    target: selected_download_source_label(
                        SelectedDownloadArtifactKind::LogConfig,
                        &logging.file.id,
                    ),
                    fact_tx: fact_tx.cloned(),
                    label: "log config",
                })
            })
            .transpose()?;
        #[cfg(not(test))]
        let acquisition_workers = ManagedBlockingWorkers::new();
        #[cfg(test)]
        let acquisition_workers = self
            .acquisition_workers
            .clone()
            .unwrap_or_else(ManagedBlockingWorkers::new);
        let _acquisition_cancellation_guard = acquisition_workers.cancellation_guard();
        let library_cache_admission = ExactLibraryCacheAdmission::bind_with_workers(
            self.managed_root(),
            acquisition_workers.clone(),
        )
        .await?;
        let library_source_pool = LibrarySourcePool::new_with_workers(acquisition_workers.clone())?;
        let prepared_asset_pipeline = if asset_index_source.is_some() {
            Some(
                prepare_asset_download_pipeline(self.managed_root(), acquisition_workers.clone())
                    .await?,
            )
        } else {
            None
        };

        if asset_index_source.is_some() {
            plan.add_done(asset_index_bytes);
            send(progress(
                "asset_index",
                1,
                1,
                Some(format!("{}.json", version.asset_index.id)),
            ));
        }

        send(progress(
            "client_jar",
            0,
            1,
            Some(format!("{version_id}.jar")),
        ));
        if let Some(logging) = version
            .logging
            .as_ref()
            .and_then(|logging| logging.client.as_ref())
        {
            send(progress("log_config", 0, 1, Some(logging.file.id.clone())));
        }
        send(progress("libraries", 0, library_jobs.len() as i32, None));
        if runtime_source.is_some() {
            send(progress(
                "java_runtime",
                0,
                0,
                Some(format!(
                    "Preparing {} (Java {})",
                    if version.java_version.component.trim().is_empty() {
                        "managed runtime".to_string()
                    } else {
                        version.java_version.component.clone()
                    },
                    version.java_version.major_version
                )),
            ));
        }

        let mut client_pipeline = client_source_spec.spawn(self.client.clone());
        let mut log_config_pipeline =
            log_config_source_spec.map(|spec| spec.spawn(self.client.clone()));
        let mut asset_pipeline = match (
            prepared_asset_pipeline,
            asset_index_source,
            asset_contribution,
        ) {
            (Some(prepared), Some(source), Some(contribution)) => {
                Some(spawn_asset_download_pipeline(
                    prepared,
                    AssetSourceAcquisitionInputs {
                        client: self.client.clone(),
                        asset_object_base_url: Arc::clone(&self.asset_object_base_url),
                        asset_index_id: version.asset_index.id.clone(),
                        asset_index_source: source,
                        fact_tx: fact_tx.cloned(),
                        plan: plan.clone(),
                        contribution,
                    },
                ))
            }
            (None, None, None) => None,
            _ => unreachable!("prepared asset source and transfer contribution must stay paired"),
        };
        let mut runtime_pipeline = match (runtime_source, runtime_contribution) {
            (Some(runtime_source), Some(contribution)) => {
                let java_version = version.java_version.clone();
                Some(self.spawn_runtime_pipeline(
                    java_version,
                    runtime_source,
                    plan.clone(),
                    contribution,
                ))
            }
            (None, None) => None,
            _ => unreachable!("runtime source and transfer contribution must stay paired"),
        };

        let total_library_jobs = library_jobs.len() as i32;
        let mut completed_library_jobs = 0;
        let mut library_proofs = Vec::with_capacity(total_library_jobs as usize);
        let mut library_sources = Vec::with_capacity(total_library_jobs as usize);
        let mut library_downloads =
            futures_util::stream::iter(library_jobs.into_iter().zip(library_contributions).map(
                |(classified, contribution)| {
                    let client = self.client.clone();
                    let fact_tx = fact_tx.cloned();
                    let source_pool = library_source_pool.clone();
                    let cache_admission = library_cache_admission.clone();
                    async move {
                        let acquisition = acquire_retained_classified_library(
                            &client,
                            classified,
                            &cache_admission,
                            &source_pool,
                            fact_tx.as_ref(),
                        )
                        .await?;
                        Ok::<_, DownloadError>((acquisition, contribution))
                    }
                },
            ))
            .buffer_unordered(library_download_concurrency());

        enum LaneEvent {
            Client(Result<AuthenticatedSelectedArtifactSource, DownloadError>),
            LogConfig(Result<AuthenticatedSelectedArtifactSource, DownloadError>),
            Library(
                Option<
                    Result<
                        (
                            RetainedClassifiedLibraryAcquisition,
                            Option<TransferPlanContribution>,
                        ),
                        DownloadError,
                    >,
                >,
            ),
            Asset(AssetDownloadPipelineEvent),
            Runtime(RuntimeEnsurePipelineEvent),
        }

        let mut client_source = None;
        let mut log_config_source = None;
        let mut client_complete = false;
        let mut log_config_complete = log_config_pipeline.is_none();
        let mut libraries_complete = false;
        let mut asset_complete = asset_pipeline.is_none();
        let mut runtime_complete = runtime_pipeline.is_none();
        let mut asset_result = None;
        let mut runtime_receipt = None;
        let mut pending_library_acquisition = None;

        #[cfg(test)]
        if self.wait_for_concurrent_terminals {
            while !(client_pipeline.is_finished()
                && asset_pipeline
                    .as_ref()
                    .is_some_and(AssetDownloadPipeline::is_finished))
            {
                tokio::task::yield_now().await;
            }
        }

        let primary_error = 'coordinator: loop {
            let event = if !client_complete && client_pipeline.is_finished() {
                LaneEvent::Client(client_pipeline.complete().await)
            } else if !log_config_complete
                && log_config_pipeline
                    .as_ref()
                    .is_some_and(SelectedSourcePipeline::is_finished)
            {
                LaneEvent::LogConfig(
                    log_config_pipeline
                        .as_mut()
                        .expect("live log-config pipeline")
                        .complete()
                        .await,
                )
            } else {
                if !libraries_complete && pending_library_acquisition.is_none() {
                    match library_downloads.next().now_or_never() {
                        Some(Some(Ok(acquisition))) => {
                            pending_library_acquisition = Some(acquisition);
                        }
                        Some(Some(Err(error))) => break 'coordinator Some(error),
                        Some(None) => libraries_complete = true,
                        None => {}
                    }
                }

                let asset_terminal = !asset_complete
                    && asset_pipeline
                        .as_ref()
                        .is_some_and(|pipeline| pipeline.is_finished());
                let runtime_terminal = !runtime_complete
                    && runtime_pipeline
                        .as_ref()
                        .is_some_and(|pipeline| pipeline.is_finished());
                match ready_concurrent_lane(
                    asset_terminal,
                    runtime_terminal,
                    pending_library_acquisition.is_some(),
                ) {
                    Some(ReadyConcurrentLane::AssetTerminal) => LaneEvent::Asset(
                        next_asset_download_pipeline_event(
                            asset_pipeline.as_mut().expect("live asset pipeline"),
                        )
                        .await,
                    ),
                    Some(ReadyConcurrentLane::RuntimeTerminal) => LaneEvent::Runtime(
                        next_runtime_pipeline_event(
                            runtime_pipeline.as_mut().expect("live runtime pipeline"),
                        )
                        .await,
                    ),
                    Some(ReadyConcurrentLane::BufferedLibrarySuccess) => {
                        LaneEvent::Library(Some(Ok(pending_library_acquisition
                            .take()
                            .expect("selected buffered library lane retains its acquisition"))))
                    }
                    None => {
                        if client_complete
                            && log_config_complete
                            && libraries_complete
                            && asset_complete
                            && runtime_complete
                        {
                            break None;
                        }
                        tokio::select! {
                            biased;
                            result = client_pipeline.complete(), if !client_complete => LaneEvent::Client(result),
                            result = async {
                                log_config_pipeline
                                    .as_mut()
                                    .expect("live log-config pipeline")
                                    .complete()
                                    .await
                            }, if !log_config_complete => LaneEvent::LogConfig(result),
                            result = library_downloads.next(), if !libraries_complete => LaneEvent::Library(result),
                            event = async {
                                next_asset_download_pipeline_event(
                                    asset_pipeline.as_mut().expect("live asset pipeline"),
                                )
                                .await
                            }, if !asset_complete => LaneEvent::Asset(event),
                            event = async {
                                next_runtime_pipeline_event(
                                    runtime_pipeline.as_mut().expect("live runtime pipeline"),
                                )
                                .await
                            }, if !runtime_complete => LaneEvent::Runtime(event),
                        }
                    }
                }
            };
            match event {
                LaneEvent::Client(Ok(source)) => {
                    client_complete = true;
                    client_source = Some(source);
                    plan.add_done(client_jar_bytes);
                    send(progress(
                        "client_jar",
                        1,
                        1,
                        Some(format!("{version_id}.jar")),
                    ));
                }
                LaneEvent::Client(Err(error)) => break 'coordinator Some(error),
                LaneEvent::LogConfig(Ok(source)) => {
                    log_config_complete = true;
                    log_config_source = Some(source);
                    plan.add_done(log_config_bytes);
                    let logging = version
                        .logging
                        .as_ref()
                        .and_then(|logging| logging.client.as_ref())
                        .expect("completed log-config pipeline has a declaration");
                    send(progress("log_config", 1, 1, Some(logging.file.id.clone())));
                }
                LaneEvent::LogConfig(Err(error)) => break 'coordinator Some(error),
                LaneEvent::Library(Some(Ok((acquisition, contribution)))) => {
                    let RetainedClassifiedLibraryAcquisition {
                        relative_path: _,
                        name,
                        observed_size,
                        proof,
                        source,
                    } = acquisition;
                    if let Some(contribution) = contribution {
                        contribution.resolve(observed_size);
                    }
                    if let Some(proof) = proof {
                        library_proofs.push(proof);
                    }
                    if let Some(source) = source {
                        library_sources.push(source);
                    }
                    plan.add_done(observed_size);
                    completed_library_jobs += 1;
                    send(progress(
                        "libraries",
                        completed_library_jobs,
                        total_library_jobs,
                        Some(name),
                    ));
                }
                LaneEvent::Library(Some(Err(error))) => break 'coordinator Some(error),
                LaneEvent::Library(None) => libraries_complete = true,
                LaneEvent::Asset(AssetDownloadPipelineEvent::Progress(progress)) => send(progress),
                LaneEvent::Asset(AssetDownloadPipelineEvent::Complete {
                    result,
                    final_progress,
                }) => match result {
                    Ok(result) => {
                        for progress in final_progress {
                            send(progress);
                        }
                        asset_complete = true;
                        asset_pipeline.take();
                        asset_result = Some(result);
                    }
                    Err(error) => {
                        asset_pipeline.take();
                        break 'coordinator Some(error);
                    }
                },
                LaneEvent::Runtime(RuntimeEnsurePipelineEvent::Progress(progress)) => {
                    send(progress)
                }
                LaneEvent::Runtime(RuntimeEnsurePipelineEvent::Complete {
                    result,
                    final_progress,
                }) => match result {
                    Ok(receipt) => {
                        for progress in final_progress {
                            send(progress);
                        }
                        runtime_complete = true;
                        runtime_pipeline.take();
                        runtime_receipt = Some(receipt);
                    }
                    Err(error) => {
                        runtime_pipeline.take();
                        break 'coordinator Some(error);
                    }
                },
            }
        };

        if let Some(error) = primary_error {
            acquisition_workers.cancel();
            drop(library_downloads);
            client_pipeline.abort();
            if let Some(pipeline) = &log_config_pipeline {
                pipeline.abort();
            }
            abort_asset_download_pipeline(&mut asset_pipeline).await;
            acquisition_workers.drain().await;
            client_pipeline.drain().await;
            if let Some(pipeline) = log_config_pipeline.as_mut() {
                pipeline.drain().await;
            }
            drop(pending_library_acquisition);
            drop(client_source);
            drop(log_config_source);
            drop(library_proofs);
            drop(library_sources);
            drop(asset_result);
            drop(runtime_receipt);
            return Err(
                settle_runtime_pipeline_after_failure(runtime_pipeline.take(), error).await,
            );
        }
        drop(library_downloads);
        acquisition_workers.drain().await;

        let client_source = client_source.expect("successful client lane returned its source");
        let library_declarations = pending_library_declarations
            .seal_streamed(library_proofs)
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "installed library declarations could not be completed: {error:?}"
                ))
            })?;
        let (asset_index_source, asset_sources) = match asset_result {
            Some(RetainedAssetsAcquisition {
                asset_index_source,
                sources,
                cache_proofs: _,
            }) => (Some(asset_index_source), sources),
            None => (None, RetainedAssetSourceSet::new()),
        };
        let source_authority = AuthenticatedVanillaInstallSources::new(
            VanillaAuthorityParts {
                version,
                environment,
                libraries: library_declarations,
                version_source: version_json_source,
                asset_index_source,
                runtime_source: runtime_receipt,
            },
            client_source,
            log_config_source,
        );
        let authority =
            authenticate_pending_known_good_install(&source_authority).map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "installed source inventory could not be derived: {error:?}"
                ))
            })?;
        let source = source_authority.into_version_bundle_source()?;
        Ok(PreparedManagedInstall {
            authority,
            version_bundle_source: source,
            asset_sources,
            library_sources,
        })
    }

    async fn acquire_vanilla_plan(
        &self,
        version_id: &str,
        version_manifest_entry: &ManifestEntry,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    ) -> Result<AuthenticatedVanillaPlan, DownloadError> {
        let AuthenticatedVanillaVersionSource {
            version,
            environment,
            version_json_source,
        } = self
            .acquire_vanilla_version_source(version_id, version_manifest_entry, fact_tx)
            .await?;
        validate_vanilla_asset_index_contract(&version)?;
        let declaration_source =
            seal_vanilla_exact_library_declarations(version_json_source, &version, &environment)
                .map_err(|error| {
                    DownloadError::ResolveManifest(format!(
                        "authenticated library declarations could not be sealed: {error:?}"
                    ))
                })?;
        let (library_declarations, version_json_source) = declaration_source.into_parts();
        let (pending_library_declarations, library_jobs) = library_declarations
            .classify_jobs(library_jobs_for(&version.libraries, &environment)?)
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "library declaration classification failed: {error:?}"
                ))
            })?;
        let asset_index_source = self.acquire_asset_index_source(&version, fact_tx).await?;
        let runtime_source = if version.java_version.major_version > 0 {
            Some(
                self.acquire_runtime_source(&version.java_version)
                    .await
                    .map_err(super::runtime::runtime_lookup_error_to_download_error)?,
            )
        } else {
            None
        };
        Ok(AuthenticatedVanillaPlan {
            version,
            environment,
            pending_library_declarations,
            library_jobs,
            version_json_source,
            asset_index_source,
            runtime_source,
        })
    }

    async fn acquire_vanilla_version_source(
        &self,
        version_id: &str,
        version_manifest_entry: &ManifestEntry,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    ) -> Result<AuthenticatedVanillaVersionSource, DownloadError> {
        let expected = ExpectedIntegrity::from_sha1(&version_manifest_entry.sha1);
        let source_target =
            selected_download_source_label(SelectedDownloadArtifactKind::VersionJson, version_id);
        let version_json_source =
            acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                client: &self.client,
                kind: SelectedDownloadArtifactKind::VersionJson,
                url: &version_manifest_entry.url,
                logical_identity: version_id,
                expected: &expected,
                max_bytes: MAX_KNOWN_GOOD_VERSION_JSON_BYTES,
                target: &source_target,
                fact_tx,
            })
            .await?;
        let version = parse_vanilla_version_source(version_json_source.bytes(), version_id)?;
        validate_vanilla_version_bundle_contracts(&version)?;
        let environment = default_environment();
        Ok(AuthenticatedVanillaVersionSource {
            version,
            environment,
            version_json_source,
        })
    }

    async fn acquire_runtime_source(
        &self,
        java_version: &crate::launch::JavaVersion,
    ) -> Result<RuntimeSourceReceipt, crate::runtime::JavaRuntimeLookupError> {
        #[cfg(test)]
        if let Some(descriptor) = &self.runtime_source {
            return acquire_test_runtime_source(java_version, descriptor).await;
        }
        #[cfg(test)]
        if self.install_manifest.is_some() {
            return authenticated_test_runtime_source(java_version);
        }
        acquire_preferred_runtime_source(java_version).await
    }

    fn spawn_runtime_pipeline(
        &self,
        java_version: crate::launch::JavaVersion,
        source_receipt: RuntimeSourceReceipt,
        plan: Arc<TransferPlan>,
        contribution: TransferPlanContribution,
    ) -> super::runtime::RuntimeEnsurePipeline {
        #[cfg(test)]
        if self.install_manifest.is_some() {
            return spawn_test_runtime_source_pipeline(source_receipt, contribution);
        }
        spawn_runtime_ensure_pipeline(
            self.managed_runtime_cache().clone(),
            java_version,
            source_receipt,
            plan,
            contribution,
        )
    }

    async fn acquire_asset_index_source(
        &self,
        version: &VersionJson,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
    ) -> Result<Option<AuthenticatedSelectedArtifactSource>, DownloadError> {
        if version.asset_index.url.trim().is_empty() {
            return Ok(None);
        }
        let expected =
            ExpectedIntegrity::from_mojang(version.asset_index.size, &version.asset_index.sha1);
        let source_target = selected_download_source_label(
            SelectedDownloadArtifactKind::AssetIndex,
            &version.asset_index.id,
        );
        let source =
            acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                client: &self.client,
                kind: SelectedDownloadArtifactKind::AssetIndex,
                url: &version.asset_index.url,
                logical_identity: &version.asset_index.id,
                expected: &expected,
                max_bytes: MAX_KNOWN_GOOD_ASSET_INDEX_BYTES,
                target: &source_target,
                fact_tx,
            })
            .await?;
        Ok(Some(source))
    }

    async fn resolve_manifest_entry(
        &self,
        version_id: &str,
    ) -> Result<ManifestEntry, DownloadError> {
        let manifest = self
            .fresh_install_manifest()
            .await
            .map_err(|error| DownloadError::ResolveManifest(error.to_string()))?;
        manifest
            .versions
            .into_iter()
            .find(|entry| entry.id == version_id)
            .map(validate_version_manifest_entry)
            .transpose()?
            .ok_or_else(|| {
                DownloadError::ResolveManifest(format!(
                    "version {version_id} not found in manifest"
                ))
            })
    }

    async fn fresh_install_manifest(&self) -> Result<VersionManifest, String> {
        #[cfg(test)]
        if let Some(manifest) = &self.install_manifest {
            return Ok(manifest.clone());
        }

        fetch_fresh_install_version_manifest().await
    }
}

pub(crate) async fn reconstruct_profile_library_declarations(
    declarations: PendingExactLibraryDeclarations,
    context: &ManagedReconstructionContext,
) -> Result<(SealedExactLibraryDeclarations, RetainedLibrarySourceSet), DownloadError> {
    let attempt = context.blocking_attempt().await;
    let result = reconstruct_profile_library_declarations_unsettled(declarations, context).await;
    context.settle_blocking_attempt(attempt, result).await
}

async fn reconstruct_profile_library_declarations_unsettled(
    declarations: PendingExactLibraryDeclarations,
    context: &ManagedReconstructionContext,
) -> Result<(SealedExactLibraryDeclarations, RetainedLibrarySourceSet), DownloadError> {
    let jobs = {
        let (libraries, environment) = declarations.profile_plan_inputs().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "profile library reconstruction contract is missing".to_string(),
            )
        })?;
        library_jobs_for(libraries, environment)?
    };
    let (pending, classified) = declarations.classify_jobs(jobs).map_err(|error| {
        DownloadError::ResolveManifest(format!(
            "profile library reconstruction classification failed: {error:?}"
        ))
    })?;
    let client = standard_minecraft_download_client();
    let mut proofs = Vec::new();
    let mut sources = RetainedLibrarySourceSet::new();
    let source_pool = context.phase_library_source_pool()?;
    for classified in classified {
        let (job, acquisition) = classified.into_parts();
        if acquisition == LibraryAcquisition::ExactDeclaration
            && (!context.retains_library_sources()
                || !context.requires_retained_exact_source(&job).await?)
        {
            continue;
        }
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::Library,
            job.relative_path.as_str(),
        );
        let source = acquire_retained_library_component_source(
            LibrarySourceRequest {
                client: &client,
                url: &job.url,
                expected: &job.expected,
                relative_path: &job.relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: &target,
                pool: &source_pool,
                fact_tx: None,
            },
            component_source_kind(job.is_native),
        )
        .await?;
        if acquisition == LibraryAcquisition::FreshStream {
            proofs.push(reconstruction_download_proof(&source)?);
        }
        if context.retains_library_sources() {
            sources.insert(source)?;
        }
    }
    let declarations = pending.seal_streamed(proofs).map_err(|error| {
        DownloadError::ResolveManifest(format!(
            "profile library reconstruction could not be completed: {error:?}"
        ))
    })?;
    Ok((declarations, sources))
}

pub(crate) async fn reconstruct_installer_library_declarations(
    sources: crate::loaders::PendingForgeReconstructionSources,
    context: &ManagedReconstructionContext,
) -> Result<
    (
        crate::loaders::BoundForgeInstallExecution,
        RetainedLibrarySourceSet,
    ),
    DownloadError,
> {
    reconstruct_installer_library_declarations_inner(sources, None, context).await
}

pub(crate) async fn reconstruct_installer_processor_sources(
    sources: crate::loaders::PendingForgeReconstructionSources,
    workspace: &crate::loaders::workspace::cleanup::ProcessorWorkspace,
    context: &ManagedReconstructionContext,
) -> Result<
    (
        crate::loaders::BoundForgeInstallExecution,
        RetainedLibrarySourceSet,
    ),
    DownloadError,
> {
    reconstruct_installer_library_declarations_inner(sources, Some(workspace), context).await
}

async fn reconstruct_installer_library_declarations_inner(
    sources: crate::loaders::PendingForgeReconstructionSources,
    workspace: Option<&crate::loaders::workspace::cleanup::ProcessorWorkspace>,
    context: &ManagedReconstructionContext,
) -> Result<
    (
        crate::loaders::BoundForgeInstallExecution,
        RetainedLibrarySourceSet,
    ),
    DownloadError,
> {
    let attempt = context.blocking_attempt().await;
    let result =
        reconstruct_installer_library_declarations_unsettled(sources, workspace, context).await;
    context.settle_blocking_attempt(attempt, result).await
}

async fn reconstruct_installer_library_declarations_unsettled(
    sources: crate::loaders::PendingForgeReconstructionSources,
    workspace: Option<&crate::loaders::workspace::cleanup::ProcessorWorkspace>,
    context: &ManagedReconstructionContext,
) -> Result<
    (
        crate::loaders::BoundForgeInstallExecution,
        RetainedLibrarySourceSet,
    ),
    DownloadError,
> {
    let (pending, jobs, mut required_execution_inputs) = sources.into_parts();
    if workspace.is_none() && !required_execution_inputs.is_empty() {
        return Err(DownloadError::ResolveManifest(
            "processor reconstruction sources require an ephemeral workspace".to_string(),
        ));
    }
    let client = standard_minecraft_download_client();
    let mut proofs = Vec::new();
    let mut retained_sources = RetainedLibrarySourceSet::new();
    let source_pool = context.phase_library_source_pool()?;
    for classified in jobs {
        let (plan, acquisition) = classified.into_parts();
        let required_by_execution = required_execution_inputs.remove(&plan.relative_path);
        let stage_in_workspace = required_by_execution;
        let kind = component_source_kind(plan.is_native);
        let cache_job = if acquisition == LibraryAcquisition::ExactDeclaration {
            Some(super::libraries::DownloadJob {
                relative_path: plan.relative_path.clone(),
                url: plan.source_url.clone().ok_or_else(|| {
                    DownloadError::ResolveManifest(
                        "installer reconstruction library source is missing".to_string(),
                    )
                })?,
                name: plan.name.clone(),
                expected: plan.expected.clone(),
                is_native: plan.is_native,
            })
        } else {
            None
        };
        let cached_source = if let Some(job) = cache_job.as_ref() {
            if stage_in_workspace && job.expected.size.is_none_or(|size| size > 128 << 20) {
                return Err(DownloadError::Integrity(
                    "processor library input exceeds its admitted size bound".to_string(),
                ));
            }
            if !context.retains_library_sources() {
                if !stage_in_workspace {
                    continue;
                }
                None
            } else if stage_in_workspace {
                context
                    .retain_exact_cache_source(job, &source_pool, kind)
                    .await?
            } else if context.requires_retained_exact_source(job).await? {
                None
            } else {
                continue;
            }
        } else {
            None
        };
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::Library,
            plan.relative_path.as_str(),
        );
        let max_bytes = if stage_in_workspace {
            128 << 20
        } else {
            LIBRARY_SOURCE_MAX_BYTES
        };
        let source = match cached_source {
            Some(source) => source,
            None => {
                acquire_retained_library_component_source(
                    LibrarySourceRequest {
                        client: &client,
                        url: plan.source_url.as_deref().ok_or_else(|| {
                            DownloadError::ResolveManifest(
                                "installer reconstruction library source is missing".to_string(),
                            )
                        })?,
                        expected: &plan.expected,
                        relative_path: &plan.relative_path,
                        max_bytes,
                        target: &target,
                        pool: &source_pool,
                        fact_tx: None,
                    },
                    kind,
                )
                .await?
            }
        };
        if stage_in_workspace {
            let path = source.relative_path().clone();
            let (reader, size, sha1) = source
                .replay()
                .map_err(|error| DownloadError::FileOperation(io::Error::other(error.to_string())))?
                .into_parts();
            workspace
                .ok_or_else(|| {
                    DownloadError::ResolveManifest(
                        "processor reconstruction workspace is missing".to_string(),
                    )
                })?
                .import_library_authenticated(&path, reader, size, sha1)
                .await
                .map_err(|error| {
                    DownloadError::FileOperation(io::Error::other(error.to_string()))
                })?;
            if acquisition == LibraryAcquisition::FreshStream {
                proofs.push(reconstruction_download_proof(&source)?);
            }
        } else if acquisition == LibraryAcquisition::FreshStream {
            proofs.push(reconstruction_download_proof(&source)?);
        }
        if context.retains_library_sources() {
            retained_sources.insert(source)?;
        }
    }
    if !required_execution_inputs.is_empty() {
        return Err(DownloadError::ResolveManifest(
            "processor reconstruction input source is missing".to_string(),
        ));
    }
    let execution = pending.complete_sources(proofs).map_err(|error| {
        DownloadError::ResolveManifest(format!(
            "installer library reconstruction could not be completed: {error}"
        ))
    })?;
    Ok((execution, retained_sources))
}

fn component_source_kind(is_native: bool) -> LibraryComponentSourceKind {
    if is_native {
        LibraryComponentSourceKind::NativeLibrary
    } else {
        LibraryComponentSourceKind::Library
    }
}

fn reconstruction_download_proof(
    source: &RetainedLibraryComponentSource,
) -> Result<ExactLibraryDownloadProof, DownloadError> {
    source.exact_download_proof().ok_or_else(|| {
        DownloadError::Integrity(
            "reconstruction source lost its authenticated network origin".to_string(),
        )
    })
}

fn version_bundle_contract(
    version_id: &str,
    projection: &ManagedComponentProjection<'_>,
) -> Result<VersionBundleContract, DownloadError> {
    if projection.component() != ManagedKnownGoodComponent::VersionBundle
        || !(2..=3).contains(&projection.entry_count())
    {
        return Err(version_bundle_install_error(
            "registered VersionBundle projection shape is invalid",
        ));
    }
    let version_json = version_bundle_member_contract(
        version_id,
        projection,
        KnownGoodArtifactKind::VersionMetadata,
    )?;
    let client_jar =
        version_bundle_member_contract(version_id, projection, KnownGoodArtifactKind::ClientJar)?;
    let log_config = projection
        .entries()
        .iter()
        .any(|projected| projected.entry().kind() == KnownGoodArtifactKind::LogConfig)
        .then(|| {
            version_bundle_member_contract(version_id, projection, KnownGoodArtifactKind::LogConfig)
        })
        .transpose()?;
    if projection.entry_count() != 2 + usize::from(log_config.is_some())
        || version_json.size > MAX_KNOWN_GOOD_VERSION_JSON_BYTES as u64
    {
        return Err(version_bundle_install_error(
            "registered VersionBundle projection members are invalid",
        ));
    }
    Ok(VersionBundleContract {
        version_json,
        client_jar,
        log_config,
    })
}

fn version_bundle_member_contract(
    version_id: &str,
    projection: &ManagedComponentProjection<'_>,
    kind: KnownGoodArtifactKind,
) -> Result<VersionBundleMemberContract, DownloadError> {
    let mut matches = projection
        .entries()
        .iter()
        .filter(|projected| projected.entry().kind() == kind);
    let projected = matches.next().ok_or_else(|| {
        version_bundle_install_error("registered VersionBundle projection member is missing")
    })?;
    if matches.next().is_some() {
        return Err(version_bundle_install_error(
            "registered VersionBundle projection member is duplicated",
        ));
    }
    let entry = projected.entry();
    let (root_name, logical_identity) = match kind {
        KnownGoodArtifactKind::VersionMetadata => (
            "versions",
            projected_member_identity(version_id, projection, kind)?,
        ),
        KnownGoodArtifactKind::ClientJar => (
            "versions",
            projected_member_identity(version_id, projection, kind)?,
        ),
        KnownGoodArtifactKind::LogConfig => (
            "assets",
            projected_optional_log_identity(projection)?.ok_or_else(|| {
                version_bundle_install_error("registered VersionBundle log projection is missing")
            })?,
        ),
        _ => {
            return Err(version_bundle_install_error(
                "registered VersionBundle projection member kind is invalid",
            ));
        }
    };
    let expected_root = match kind {
        KnownGoodArtifactKind::VersionMetadata | KnownGoodArtifactKind::ClientJar => {
            KnownGoodRoot::Versions
        }
        KnownGoodArtifactKind::LogConfig => KnownGoodRoot::Assets,
        _ => unreachable!(),
    };
    if entry.root() != &expected_root {
        return Err(version_bundle_install_error(
            "registered VersionBundle projection root is invalid",
        ));
    }
    let (digest, size) = match entry.integrity() {
        KnownGoodIntegrity::Sha1 { digest, size }
        | KnownGoodIntegrity::ExactBytes { digest, size }
            if *size > 0 && *size <= MAX_TIER2_ARTIFACT_BYTES =>
        {
            (digest.as_str().to_string(), *size)
        }
        _ => {
            return Err(version_bundle_install_error(
                "registered VersionBundle projection integrity is invalid",
            ));
        }
    };
    Ok(VersionBundleMemberContract {
        kind,
        root_name,
        relative_path: entry.path().as_str().to_string(),
        logical_identity,
        digest,
        size,
    })
}

async fn retain_exact_local_version_bundle_sources(
    managed_root: ManagedDir,
    contract: &VersionBundleContract,
) -> Result<ExactLocalVersionBundleSources, DownloadError> {
    let contract = contract.clone();
    run_publication_blocking(move || {
        Ok(ExactLocalVersionBundleSources {
            version_json: read_exact_local_version_bundle_member(
                &managed_root,
                &contract.version_json,
            )?,
            client_jar: read_exact_local_version_bundle_member(
                &managed_root,
                &contract.client_jar,
            )?,
            log_config: contract
                .log_config
                .as_ref()
                .map(|member| read_exact_local_version_bundle_member(&managed_root, member))
                .transpose()?
                .flatten(),
        })
    })
    .await
    .map_err(|_| {
        DownloadError::FileOperation(io::Error::other(
            "registered VersionBundle local source task stopped",
        ))
    })?
}

fn read_exact_local_version_bundle_member(
    managed_root: &ManagedDir,
    contract: &VersionBundleMemberContract,
) -> Result<Option<AuthenticatedVersionBundleMemberSource>, DownloadError> {
    validate_existing_managed_target_path(
        managed_root,
        contract.root_name,
        &contract.relative_path,
    )
    .map_err(|_| {
        DownloadError::FileOperation(io::Error::other(
            "registered VersionBundle local source path is ambiguous",
        ))
    })?;
    let Some((parent, name)) =
        open_managed_target_parent(managed_root, contract.root_name, &contract.relative_path)
            .map_err(|_| {
                DownloadError::FileOperation(io::Error::other(
                    "registered VersionBundle local source parent is unavailable",
                ))
            })?
    else {
        return Ok(None);
    };
    let Some(guard) = parent.inspect_regular_file(&name).map_err(|_| {
        DownloadError::FileOperation(io::Error::other(
            "registered VersionBundle local source is not a stable regular file",
        ))
    })?
    else {
        return Ok(None);
    };
    if guard.size() != contract.size {
        return Ok(None);
    }
    let bytes = parent
        .read_guarded_file_bounded(&name, &guard, contract.size)
        .map_err(|_| {
            DownloadError::FileOperation(io::Error::other(
                "registered VersionBundle local source read failed",
            ))
        })?;
    let source = AuthenticatedVersionBundleMemberSource::from_local(
        contract.kind,
        contract.logical_identity.clone(),
        bytes,
    )?;
    if source.observed_size() != contract.size
        || !source
            .observed_sha1()
            .eq_ignore_ascii_case(&contract.digest)
    {
        return Ok(None);
    }
    Ok(Some(source))
}

fn expected_integrity(contract: &VersionBundleMemberContract) -> ExpectedIntegrity {
    ExpectedIntegrity {
        size: Some(contract.size),
        sha1: Some(contract.digest.clone()),
    }
}

fn validate_version_bundle_contract_against_metadata(
    contract: &VersionBundleContract,
    version: &VersionJson,
) -> Result<(), DownloadError> {
    let client = version.downloads.client.as_ref().ok_or_else(|| {
        version_bundle_install_error("authenticated VersionBundle client contract is missing")
    })?;
    if client.url.trim().is_empty()
        || u64::try_from(client.size).ok() != Some(contract.client_jar.size)
        || !client
            .sha1
            .eq_ignore_ascii_case(&contract.client_jar.digest)
    {
        return Err(version_bundle_install_error(
            "authenticated VersionBundle client contract does not match the projection",
        ));
    }
    let metadata_log = version
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
        .map(|logging| &logging.file);
    match (&contract.log_config, metadata_log) {
        (None, None) => Ok(()),
        (Some(contract), Some(file))
            if file.id == contract.logical_identity
                && !file.url.trim().is_empty()
                && u64::try_from(file.size).ok() == Some(contract.size)
                && file.sha1.eq_ignore_ascii_case(&contract.digest) =>
        {
            Ok(())
        }
        _ => Err(version_bundle_install_error(
            "authenticated VersionBundle log contract does not match the projection",
        )),
    }
}

fn exact_version_bundle_source_limit(size: i64, label: &str) -> Result<usize, DownloadError> {
    u64::try_from(size)
        .ok()
        .filter(|size| *size > 0 && *size <= MAX_TIER2_ARTIFACT_BYTES)
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(|| {
            DownloadError::ResolveManifest(format!(
                "authenticated {label} exceeds the version bundle source limit"
            ))
        })
}

fn version_bundle_sources_match_projection(
    version_id: &str,
    projection: &ManagedComponentProjection<'_>,
    members: &[AuthenticatedVersionBundleMemberSource],
) -> bool {
    if projection.component() != ManagedKnownGoodComponent::VersionBundle
        || projection.entry_count() != members.len()
        || !(2..=3).contains(&members.len())
    {
        return false;
    }
    members
        .iter()
        .all(|source| source_matches_version_bundle_entry(projection, version_id, source))
        && projection.entries().iter().all(|projected| {
            members
                .iter()
                .filter(|source| source.kind() == projected.entry().kind())
                .count()
                == 1
        })
}

fn source_matches_version_bundle_entry(
    projection: &ManagedComponentProjection<'_>,
    version_id: &str,
    source: &AuthenticatedVersionBundleMemberSource,
) -> bool {
    let Some(projected) = projection.entries().iter().find(|projected| {
        let entry = projected.entry();
        entry.kind() == source.kind()
    }) else {
        return false;
    };
    let expected_identity = match source.kind() {
        KnownGoodArtifactKind::VersionMetadata
            if projected.entry().root() == &KnownGoodRoot::Versions
                && projected.entry().path().as_str()
                    == format!("{version_id}/{version_id}.json") =>
        {
            version_id
        }
        KnownGoodArtifactKind::ClientJar
            if projected.entry().root() == &KnownGoodRoot::Versions
                && projected.entry().path().as_str()
                    == format!("{version_id}/{version_id}.jar") =>
        {
            version_id
        }
        KnownGoodArtifactKind::LogConfig if projected.entry().root() == &KnownGoodRoot::Assets => {
            let Some(identity) = projected
                .entry()
                .path()
                .as_str()
                .strip_prefix("log_configs/")
                .filter(|identity| !identity.is_empty() && !identity.contains('/'))
            else {
                return false;
            };
            identity
        }
        _ => return false,
    };
    let (digest, size) = match projected.entry().integrity() {
        KnownGoodIntegrity::Sha1 { digest, size }
        | KnownGoodIntegrity::ExactBytes { digest, size } => (digest, *size),
        KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => return false,
    };
    source.logical_identity() == expected_identity
        && source.observed_size() == size
        && u64::try_from(source.bytes().len()).is_ok_and(|observed| observed == size)
        && source.observed_sha1().eq_ignore_ascii_case(digest.as_str())
}

fn projected_member_identity(
    version_id: &str,
    projection: &ManagedComponentProjection<'_>,
    kind: KnownGoodArtifactKind,
) -> Result<String, DownloadError> {
    let expected_path = match kind {
        KnownGoodArtifactKind::VersionMetadata => format!("{version_id}/{version_id}.json"),
        KnownGoodArtifactKind::ClientJar => format!("{version_id}/{version_id}.jar"),
        _ => {
            return Err(version_bundle_install_error(
                "local version bundle member kind is invalid",
            ));
        }
    };
    let matches = projection
        .entries()
        .iter()
        .filter(|projected| {
            let entry = projected.entry();
            entry.kind() == kind
                && entry.root() == &KnownGoodRoot::Versions
                && entry.path().as_str() == expected_path
        })
        .count();
    if matches != 1 {
        return Err(version_bundle_install_error(
            "local version bundle projection topology is invalid",
        ));
    }
    Ok(version_id.to_string())
}

fn projected_optional_log_identity(
    projection: &ManagedComponentProjection<'_>,
) -> Result<Option<String>, DownloadError> {
    let mut logs = projection
        .entries()
        .iter()
        .filter(|projected| projected.entry().kind() == KnownGoodArtifactKind::LogConfig);
    let Some(projected) = logs.next() else {
        return Ok(None);
    };
    if logs.next().is_some() || projected.entry().root() != &KnownGoodRoot::Assets {
        return Err(version_bundle_install_error(
            "local version bundle log projection is invalid",
        ));
    }
    projected
        .entry()
        .path()
        .as_str()
        .strip_prefix("log_configs/")
        .filter(|identity| !identity.is_empty() && !identity.contains('/'))
        .map(str::to_string)
        .map(Some)
        .ok_or_else(|| version_bundle_install_error("local version bundle log identity is invalid"))
}

fn validate_install_version_id(version_id: &str) -> Result<(), DownloadError> {
    let json_name = format!("{version_id}.json");
    if version_id != version_id.trim()
        || PortableFileName::new_exact(version_id).is_err()
        || PortableFileName::new_exact(&json_name).is_err()
    {
        return Err(DownloadError::ResolveManifest(
            "invalid Minecraft version identity".to_string(),
        ));
    }
    Ok(())
}

fn validate_vanilla_version_bundle_contracts(version: &VersionJson) -> Result<(), DownloadError> {
    let client = version.downloads.client.as_ref().ok_or_else(|| {
        DownloadError::ResolveManifest(
            "authenticated version has no exact client artifact".to_string(),
        )
    })?;
    if client.url.trim().is_empty() {
        return Err(DownloadError::ResolveManifest(
            "authenticated version has no client source".to_string(),
        ));
    }
    validate_exact_mojang_contract(client.size, &client.sha1, "client")?;
    if let Some(logging) = version
        .logging
        .as_ref()
        .and_then(|logging| logging.client.as_ref())
    {
        if PortableFileName::new_exact(&logging.file.id).is_err()
            || logging.file.url.trim().is_empty()
        {
            return Err(DownloadError::ResolveManifest(
                "authenticated version has an invalid log config source".to_string(),
            ));
        }
        validate_exact_mojang_contract(logging.file.size, &logging.file.sha1, "log config")?;
    }
    Ok(())
}

fn validate_vanilla_asset_index_contract(version: &VersionJson) -> Result<(), DownloadError> {
    let asset_index = &version.asset_index;
    let absent_asset_index = asset_index.id.is_empty()
        && asset_index.url.is_empty()
        && asset_index.sha1.is_empty()
        && asset_index.size == 0
        && asset_index.total_size == 0;
    if absent_asset_index {
        return Ok(());
    }
    let index_name = format!("{}.json", asset_index.id);
    if asset_index.id.trim().is_empty()
        || asset_index.url.trim().is_empty()
        || asset_index.size < 0
        || asset_index.total_size < 0
        || PortableFileName::new_exact(&asset_index.id).is_err()
        || PortableFileName::new_exact(&index_name).is_err()
    {
        return Err(DownloadError::ResolveManifest(
            "authenticated version has an invalid asset index source".to_string(),
        ));
    }
    if asset_index.sha1.len() != 40
        || !asset_index
            .sha1
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(DownloadError::ResolveManifest(
            "authenticated version has an invalid asset index checksum".to_string(),
        ));
    }
    Ok(())
}

fn validate_exact_mojang_contract(
    size: i64,
    sha1: &str,
    artifact: &str,
) -> Result<(), DownloadError> {
    let expected = ExpectedIntegrity::from_mojang(size, sha1);
    if expected.size.is_none()
        || expected.sha1.as_deref().is_none_or(|sha1| {
            sha1.len() != 40 || !sha1.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    {
        return Err(DownloadError::ResolveManifest(format!(
            "authenticated version has no exact {artifact} contract"
        )));
    }
    Ok(())
}

fn validate_version_manifest_entry(entry: ManifestEntry) -> Result<ManifestEntry, DownloadError> {
    let expected = ExpectedIntegrity::from_sha1(&entry.sha1);
    if entry.url.trim().is_empty() || !expected.has_checksum() {
        return Err(DownloadError::ResolveManifest(
            "version manifest entry has invalid source metadata".to_string(),
        ));
    }
    Ok(entry)
}

fn registered_version_metadata_manifest_entry(
    manifest: VersionManifest,
    version_id: &str,
    contract: &VersionBundleMemberContract,
) -> Result<ManifestEntry, RegisteredVersionBundleSourceError> {
    let entry = manifest
        .versions
        .into_iter()
        .find(|entry| entry.id == version_id)
        .ok_or(RegisteredVersionBundleSourceError::Authority)
        .and_then(|entry| {
            validate_version_manifest_entry(entry)
                .map_err(|_| RegisteredVersionBundleSourceError::Authority)
        })?;
    if !entry.sha1.eq_ignore_ascii_case(&contract.digest) {
        return Err(RegisteredVersionBundleSourceError::Authority);
    }
    Ok(entry)
}

fn parse_vanilla_version_source(
    bytes: &[u8],
    expected_version_id: &str,
) -> Result<VersionJson, DownloadError> {
    let mut version = serde_json::from_slice::<VersionJson>(bytes)?;
    if version.id != expected_version_id
        || !version.inherits_from.is_empty()
        || version.materialized
    {
        return Err(DownloadError::ResolveManifest(
            "version metadata identity does not match the selected manifest entry".to_string(),
        ));
    }
    if version.asset_index.id.is_empty() && !version.assets.is_empty() {
        version.asset_index.id = version.assets.clone();
    }
    version.java_version =
        effective_java_version_for(&version.id, &version.kind, &version.java_version);
    Ok(version)
}

pub(crate) fn prepare_local_managed_install(
    authority: PendingKnownGoodInstallAuthority,
    version_json: Vec<u8>,
    client_jar: Vec<u8>,
    log_config: Option<Vec<u8>>,
    library_sources: Vec<RetainedLibraryComponentSource>,
) -> Result<PreparedManagedInstall, DownloadError> {
    let source = {
        let projection = authority.version_bundle_projection().map_err(|_| {
            version_bundle_install_error("version bundle projection could not be derived")
        })?;
        AuthenticatedVersionBundleSource::from_local_projection(
            authority.version_id().to_string(),
            &projection,
            version_json,
            client_jar,
            log_config,
        )?
    };
    Ok(PreparedManagedInstall {
        authority,
        version_bundle_source: source,
        asset_sources: RetainedAssetSourceSet::new(),
        library_sources,
    })
}

pub(crate) async fn publish_prepared_managed_install(
    managed_root: PathBuf,
    prepared: PreparedManagedInstall,
) -> Result<KnownGoodInstallReceipt, DownloadError> {
    let observer_key = prepared.authority.version_id().to_string();
    #[cfg(test)]
    let rollback_component =
        take_managed_install_component_rollback_after_first_row_for_test(&observer_key);
    let owner = tokio::spawn(async move {
        let PreparedManagedInstall {
            authority,
            version_bundle_source,
            asset_sources,
            library_sources,
        } = prepared;
        let lease = acquire_managed_install_publication_lease(managed_root, &observer_key).await?;
        match publish_managed_projection_sequence(
            lease,
            &authority,
            asset_sources.into_sources(),
            library_sources,
            version_bundle_source,
            #[cfg(test)]
            rollback_component,
        )
        .await
        {
            Ok(ManagedProjectionSequenceOutcome::Committed(_lease)) => {
                Ok(authority.seal_after_version_bundle_commit())
            }
            Ok(ManagedProjectionSequenceOutcome::RolledBack(component)) => Err(
                managed_projection_sequence_rolled_back_install_error(component),
            ),
            Err(error) => Err(managed_projection_sequence_install_error(error.component())),
        }
    });
    owner.await.map_err(managed_install_owner_error)?
}

enum ManagedProjectionSequenceOutcome {
    Committed(ManagedRootPublicationLease),
    RolledBack(ManagedKnownGoodComponent),
}

enum ManagedProjectionSequenceError {
    Projection(ManagedKnownGoodComponent),
    Component(ManagedKnownGoodComponent),
    VersionBundle,
}

impl ManagedProjectionSequenceError {
    fn component(&self) -> ManagedKnownGoodComponent {
        match self {
            Self::Projection(component) | Self::Component(component) => *component,
            Self::VersionBundle => ManagedKnownGoodComponent::VersionBundle,
        }
    }
}

async fn publish_managed_projection_sequence(
    lease: ManagedRootPublicationLease,
    authority: &PendingKnownGoodInstallAuthority,
    asset_sources: Vec<RetainedAssetComponentSource>,
    library_sources: Vec<RetainedLibraryComponentSource>,
    version_bundle_source: AuthenticatedVersionBundleSource,
    #[cfg(test)] rollback_component: Option<ManagedComponentKind>,
) -> Result<ManagedProjectionSequenceOutcome, ManagedProjectionSequenceError> {
    let assets = authority
        .component_projection(ManagedKnownGoodComponent::Assets)
        .map_err(|_| {
            ManagedProjectionSequenceError::Projection(ManagedKnownGoodComponent::Assets)
        })?;
    #[cfg(test)]
    let assets = if rollback_component == Some(ManagedComponentKind::Assets) {
        publish_managed_component_effect_rolling_back_after_first_row_for_test(
            lease,
            assets,
            ManagedComponentKind::Assets,
            asset_sources,
        )
        .await
    } else {
        publish_managed_component_effect(lease, assets, ManagedComponentKind::Assets, asset_sources)
            .await
    };
    #[cfg(not(test))]
    let assets = publish_managed_component_effect(
        lease,
        assets,
        ManagedComponentKind::Assets,
        asset_sources,
    )
    .await;
    let lease = match assets
        .map_err(|_| ManagedProjectionSequenceError::Component(ManagedKnownGoodComponent::Assets))?
    {
        ManagedComponentLifecycleOutcome::Committed(receipt) => receipt.into_lease(),
        ManagedComponentLifecycleOutcome::RolledBack(_) => {
            return Ok(ManagedProjectionSequenceOutcome::RolledBack(
                ManagedKnownGoodComponent::Assets,
            ));
        }
    };
    let libraries = authority
        .component_projection(ManagedKnownGoodComponent::Libraries)
        .map_err(|_| {
            ManagedProjectionSequenceError::Projection(ManagedKnownGoodComponent::Libraries)
        })?;
    #[cfg(test)]
    let libraries = if rollback_component == Some(ManagedComponentKind::Libraries) {
        publish_managed_component_effect_rolling_back_after_first_row_for_test(
            lease,
            libraries,
            ManagedComponentKind::Libraries,
            library_sources,
        )
        .await
    } else {
        publish_managed_component_effect(
            lease,
            libraries,
            ManagedComponentKind::Libraries,
            library_sources,
        )
        .await
    };
    #[cfg(not(test))]
    let libraries = publish_managed_component_effect(
        lease,
        libraries,
        ManagedComponentKind::Libraries,
        library_sources,
    )
    .await;
    let lease = match libraries.map_err(|_| {
        ManagedProjectionSequenceError::Component(ManagedKnownGoodComponent::Libraries)
    })? {
        ManagedComponentLifecycleOutcome::Committed(receipt) => receipt.into_lease(),
        ManagedComponentLifecycleOutcome::RolledBack(_) => {
            return Ok(ManagedProjectionSequenceOutcome::RolledBack(
                ManagedKnownGoodComponent::Libraries,
            ));
        }
    };
    let publication = {
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|_| {
                ManagedProjectionSequenceError::Projection(ManagedKnownGoodComponent::VersionBundle)
            })?;
        publish_version_bundle(lease, version_bundle_source, projection).await
    };
    let settlement = settle_version_bundle_publication(publication)
        .await
        .map_err(|_| ManagedProjectionSequenceError::VersionBundle)?;
    match settlement {
        VersionBundleTransactionSettledOutcome::Committed(lease) => {
            Ok(ManagedProjectionSequenceOutcome::Committed(lease))
        }
        VersionBundleTransactionSettledOutcome::RolledBack { .. } => Ok(
            ManagedProjectionSequenceOutcome::RolledBack(ManagedKnownGoodComponent::VersionBundle),
        ),
    }
}

fn managed_projection_sequence_install_error(
    component: ManagedKnownGoodComponent,
) -> DownloadError {
    version_bundle_install_error(format!(
        "managed {} publication failed",
        managed_projection_sequence_component_name(component)
    ))
}

fn managed_projection_sequence_rolled_back_install_error(
    component: ManagedKnownGoodComponent,
) -> DownloadError {
    version_bundle_install_error(format!(
        "managed {} publication rolled back",
        managed_projection_sequence_component_name(component)
    ))
}

fn managed_projection_sequence_component_name(
    component: ManagedKnownGoodComponent,
) -> &'static str {
    match component {
        ManagedKnownGoodComponent::VersionBundle => "VersionBundle",
        ManagedKnownGoodComponent::Libraries => "Libraries",
        ManagedKnownGoodComponent::Assets => "Assets",
    }
}

async fn acquire_managed_install_publication_lease(
    managed_root: PathBuf,
    _version_id: &str,
) -> Result<ManagedRootPublicationLease, DownloadError> {
    let root = run_publication_blocking(move || {
        std::fs::create_dir_all(&managed_root)?;
        ManagedDir::open_root(&managed_root)
    })
    .await
    .map_err(|_| version_bundle_install_error("version bundle root task stopped"))?
    .map_err(|_| version_bundle_install_error("version bundle root is unavailable"))?;
    #[cfg(test)]
    notify_managed_install_lease_wait_for_test(_version_id);
    ManagedRootPublicationLease::acquire(root)
        .await
        .map_err(|_| version_bundle_install_error("version bundle publication lock failed"))
}

#[cfg(test)]
static MANAGED_INSTALL_LEASE_WAIT_OBSERVERS: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, tokio::sync::oneshot::Sender<()>>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
static MANAGED_INSTALL_COMPONENT_ROLLBACKS_AFTER_FIRST_ROW: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<String, ManagedComponentKind>>,
> = std::sync::OnceLock::new();

#[cfg(test)]
pub(super) fn roll_back_managed_install_component_after_first_row_for_test(
    version_id: &str,
    component: ManagedComponentKind,
) {
    let replaced = MANAGED_INSTALL_COMPONENT_ROLLBACKS_AFTER_FIRST_ROW
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(version_id.to_string(), component);
    assert!(replaced.is_none(), "install rollback hook must be unique");
}

#[cfg(test)]
fn take_managed_install_component_rollback_after_first_row_for_test(
    version_id: &str,
) -> Option<ManagedComponentKind> {
    MANAGED_INSTALL_COMPONENT_ROLLBACKS_AFTER_FIRST_ROW
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(version_id)
}

#[cfg(test)]
pub(super) fn observe_managed_install_lease_wait_for_test(
    version_id: &str,
) -> tokio::sync::oneshot::Receiver<()> {
    let (reached_tx, reached_rx) = tokio::sync::oneshot::channel();
    let replaced = MANAGED_INSTALL_LEASE_WAIT_OBSERVERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(version_id.to_string(), reached_tx);
    assert!(replaced.is_none(), "lease wait observer must be unique");
    reached_rx
}

#[cfg(test)]
fn notify_managed_install_lease_wait_for_test(version_id: &str) {
    let observer = MANAGED_INSTALL_LEASE_WAIT_OBSERVERS
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(version_id);
    if let Some(observer) = observer {
        let _ = observer.send(());
    }
}

fn version_bundle_install_error(message: impl Into<String>) -> DownloadError {
    DownloadError::ResolveManifest(message.into())
}

fn selected_source_task_error(error: tokio::task::JoinError, label: &str) -> DownloadError {
    let reason = if error.is_cancelled() {
        "cancelled"
    } else if error.is_panic() {
        "panicked"
    } else {
        "failed"
    };
    DownloadError::FileOperation(io::Error::other(format!("{label} source task {reason}")))
}

fn managed_install_owner_error(error: tokio::task::JoinError) -> DownloadError {
    let reason = if error.is_cancelled() {
        "cancelled"
    } else if error.is_panic() {
        "panicked"
    } else {
        "failed"
    };
    version_bundle_install_error(format!("managed install publication task {reason}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct BlockingWorkerGate {
        state: Arc<(std::sync::Mutex<bool>, std::sync::Condvar)>,
    }

    impl BlockingWorkerGate {
        fn new() -> Self {
            Self {
                state: Arc::new((std::sync::Mutex::new(false), std::sync::Condvar::new())),
            }
        }

        fn shared(&self) -> Arc<(std::sync::Mutex<bool>, std::sync::Condvar)> {
            Arc::clone(&self.state)
        }

        fn release(&self) {
            let (lock, condition) = &*self.state;
            *lock.lock().expect("worker release lock") = true;
            condition.notify_all();
        }
    }

    impl Drop for BlockingWorkerGate {
        fn drop(&mut self) {
            self.release();
        }
    }

    fn exact_contract_version() -> serde_json::Value {
        serde_json::json!({
            "id": "contract",
            "downloads": { "client": {
                "url": "https://example.invalid/client.jar",
                "sha1": "1111111111111111111111111111111111111111",
                "size": 7
            }}
        })
    }

    #[test]
    fn exact_contract_preflight_rejects_invalid_client_metadata() {
        for (field, replacement) in [
            ("url", Some(serde_json::json!(""))),
            ("sha1", Some(serde_json::json!("bad"))),
            ("size", Some(serde_json::json!(0))),
            ("size", Some(serde_json::json!(-1))),
            ("size", None),
        ] {
            let mut value = exact_contract_version();
            if let Some(replacement) = replacement {
                value["downloads"]["client"][field] = replacement;
            } else {
                value["downloads"]["client"]
                    .as_object_mut()
                    .expect("client object")
                    .remove(field);
            }
            let version: VersionJson = serde_json::from_value(value).expect("version metadata");
            assert!(validate_vanilla_version_bundle_contracts(&version).is_err());
        }
    }

    #[test]
    fn exact_contract_preflight_distinguishes_absent_and_malformed_logging() {
        for logging in [serde_json::Value::Null, serde_json::json!({})] {
            let mut value = exact_contract_version();
            if !logging.is_null() {
                value["logging"] = logging;
            }
            let version: VersionJson = serde_json::from_value(value).expect("version metadata");
            validate_vanilla_version_bundle_contracts(&version).expect("absent logging");
        }

        for file in [
            serde_json::json!({
                "id": "../escape.xml", "url": "https://example.invalid/log.xml",
                "sha1": "2222222222222222222222222222222222222222", "size": 4
            }),
            serde_json::json!({
                "id": "log.xml", "url": "", "sha1": "2222222222222222222222222222222222222222", "size": 4
            }),
            serde_json::json!({
                "id": "log.xml", "url": "https://example.invalid/log.xml",
                "sha1": "2222222222222222222222222222222222222222", "size": 0
            }),
            serde_json::json!({
                "id": "log.xml", "url": "https://example.invalid/log.xml",
                "sha1": "2222222222222222222222222222222222222222", "size": -1
            }),
        ] {
            let mut value = exact_contract_version();
            value["logging"] = serde_json::json!({
                "client": { "argument": "", "file": file, "type": "log4j2-xml" }
            });
            let version: VersionJson = serde_json::from_value(value).expect("version metadata");
            assert!(validate_vanilla_version_bundle_contracts(&version).is_err());
        }
    }

    #[test]
    fn parses_vanilla_metadata_only_for_the_selected_identity() {
        let bytes = br#"{"id":"1.21.1","type":"release","assets":"legacy"}"#;
        let version = parse_vanilla_version_source(bytes, "1.21.1").expect("valid source");

        assert_eq!(version.id, "1.21.1");
        assert_eq!(version.asset_index.id, "legacy");
        assert!(parse_vanilla_version_source(bytes, "1.21.2").is_err());
    }

    #[test]
    fn rejects_unverified_manifest_entry_source_metadata() {
        let entry = ManifestEntry {
            id: "1.21.1".to_string(),
            kind: "release".to_string(),
            url: "https://example.invalid/version.json".to_string(),
            time: String::new(),
            release_time: String::new(),
            sha1: "not-a-sha1".to_string(),
            compliance_level: 1,
        };

        assert!(validate_version_manifest_entry(entry).is_err());
    }

    #[test]
    fn registered_metadata_discovery_requires_the_pinned_projection_digest() {
        const VERSION_ID: &str = "registered-metadata";
        const PINNED_SHA1: &str = "1111111111111111111111111111111111111111";
        let contract = VersionBundleMemberContract {
            kind: KnownGoodArtifactKind::VersionMetadata,
            root_name: "versions",
            relative_path: format!("{VERSION_ID}/{VERSION_ID}.json"),
            logical_identity: VERSION_ID.to_string(),
            digest: PINNED_SHA1.to_string(),
            size: 7,
        };
        let manifest = |sha1: &str| {
            serde_json::from_value::<VersionManifest>(serde_json::json!({
                "latest": { "release": VERSION_ID, "snapshot": VERSION_ID },
                "versions": [{
                    "id": VERSION_ID,
                    "type": "release",
                    "url": "https://example.invalid/version.json",
                    "sha1": sha1
                }]
            }))
            .expect("version manifest")
        };

        assert_eq!(
            registered_version_metadata_manifest_entry(
                manifest("2222222222222222222222222222222222222222"),
                VERSION_ID,
                &contract,
            )
            .expect_err("manifest drift must fail"),
            RegisteredVersionBundleSourceError::Authority
        );
        assert_eq!(
            registered_version_metadata_manifest_entry(
                manifest(PINNED_SHA1),
                VERSION_ID,
                &contract,
            )
            .expect("pinned manifest entry")
            .sha1,
            PINNED_SHA1
        );
    }

    #[test]
    fn local_version_bundle_source_requires_exact_projected_members() {
        let version_id = "local-bundle";
        let version_json = br#"{"id":"local-bundle"}"#.to_vec();
        let client_jar = b"exact local client".to_vec();
        let log_config = b"exact local log".to_vec();
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id,
            &version_json,
            &client_jar,
            Some(("client.xml", &log_config)),
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");

        let source = AuthenticatedVersionBundleSource::from_local_projection(
            version_id.to_string(),
            &projection,
            version_json.clone(),
            client_jar.clone(),
            Some(log_config.clone()),
        )
        .expect("exact local source");
        assert!(source.matches_projection(&projection));

        let mut corrupt_client = client_jar;
        corrupt_client[0] ^= 0xff;
        assert!(
            AuthenticatedVersionBundleSource::from_local_projection(
                version_id.to_string(),
                &projection,
                version_json,
                corrupt_client,
                Some(log_config),
            )
            .is_err()
        );
    }

    #[test]
    fn terminal_lanes_precede_buffered_library_success() {
        let cases = [
            ((false, false, false), None),
            (
                (false, false, true),
                Some(ReadyConcurrentLane::BufferedLibrarySuccess),
            ),
            (
                (false, true, false),
                Some(ReadyConcurrentLane::RuntimeTerminal),
            ),
            (
                (false, true, true),
                Some(ReadyConcurrentLane::RuntimeTerminal),
            ),
            (
                (true, false, false),
                Some(ReadyConcurrentLane::AssetTerminal),
            ),
            (
                (true, false, true),
                Some(ReadyConcurrentLane::AssetTerminal),
            ),
            (
                (true, true, false),
                Some(ReadyConcurrentLane::AssetTerminal),
            ),
            ((true, true, true), Some(ReadyConcurrentLane::AssetTerminal)),
        ];

        for ((asset, runtime, library), expected) in cases {
            assert_eq!(ready_concurrent_lane(asset, runtime, library), expected);
        }
    }

    #[test]
    fn asset_cache_mapping_preserves_cancellation_and_task_failure() {
        assert!(matches!(
            asset_cache_error(
                crate::managed_component_cache::ManagedComponentExactCacheError::Admission
            ),
            DownloadError::Integrity(_)
        ));
        assert!(matches!(
            asset_cache_error(
                crate::managed_component_cache::ManagedComponentExactCacheError::Cancelled
            ),
            DownloadError::FileOperation(error)
                if error.kind() == std::io::ErrorKind::Interrupted
        ));
        assert!(matches!(
            asset_cache_error(
                crate::managed_component_cache::ManagedComponentExactCacheError::TaskStopped
            ),
            DownloadError::FileOperation(error) if error.kind() == std::io::ErrorKind::Other
        ));
    }

    #[tokio::test]
    async fn dropped_reconstruction_attempt_holds_serialization_until_workers_drain() {
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let entered_tx = Arc::new(std::sync::Mutex::new(Some(entered_tx)));
        let release = BlockingWorkerGate::new();
        let hook_release = release.shared();
        let workers =
            ManagedBlockingWorkers::new_with_checkpoint_hook(Arc::new(move |checkpoint| {
                if checkpoint != crate::managed_blocking::ManagedBlockingCheckpoint::SourceSpool {
                    return;
                }
                let Some(entered) = entered_tx
                    .lock()
                    .expect("reconstruction checkpoint lock")
                    .take()
                else {
                    return;
                };
                let _ = entered.send(());
                let (lock, condition) = &*hook_release;
                let released = lock.lock().expect("reconstruction release lock");
                drop(
                    condition
                        .wait_while(released, |released| !*released)
                        .expect("reconstruction release wait"),
                );
            }));
        let setup_attempt = workers.attempt_guard();
        let temporary = tempfile::tempdir().expect("reconstruction root");
        let managed_root = ManagedDir::open_root(temporary.path()).expect("managed root");
        let context = ManagedReconstructionContext {
            mode: ManagedReconstructionMode::Libraries(ManagedLibrariesReconstructionContext {
                source_pool: LibrarySourcePool::new_with_workers(workers.clone())
                    .expect("library source pool"),
                cache_admission: ExactLibraryCacheAdmission::bind_guarded_with_workers(
                    managed_root,
                    workers.clone(),
                )
                .await
                .expect("library cache admission"),
                cache_proofs: Arc::new(std::sync::Mutex::new(
                    AuthenticatedLibraryCacheProofSet::default(),
                )),
            }),
            workers: workers.clone(),
            blocking_operation: Arc::new(tokio::sync::Mutex::new(())),
        };
        workers.drain().await;
        setup_attempt.disarm();

        let bytes = b"gated reconstruction source".to_vec();
        let source = AuthenticatedLocalLibraryBytes::new(
            crate::portable_path::PortableRelativePath::new("org/example/gated/1/gated-1.jar")
                .expect("library source path"),
            LibraryComponentSourceKind::Library,
            bytes.clone(),
            bytes.len() as u64,
            Sha1::digest(&bytes).into(),
        )
        .expect("authenticated local source");
        let first_context = context.clone();
        let first =
            tokio::spawn(async move { first_context.retain_local_sources(vec![source]).await });
        tokio::time::timeout(std::time::Duration::from_secs(1), entered_rx)
            .await
            .expect("source worker must reach checkpoint")
            .expect("source worker entered checkpoint");

        first.abort();
        let first_error = match first.await {
            Err(error) => error,
            Ok(_) => panic!("first attempt must be aborted"),
        };
        assert!(first_error.is_cancelled());
        assert!(workers.is_cancelled());

        let second_context = context.clone();
        let mut second =
            tokio::spawn(async move { second_context.retain_local_sources(Vec::new()).await });
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(25), &mut second)
                .await
                .is_err(),
            "the drain monitor must retain reconstruction serialization"
        );

        release.release();
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            workers.drain().await;
            second
                .await
                .expect("second reconstruction task")
                .expect("second reconstruction attempt");
        })
        .await
        .expect("drain monitor must release reconstruction serialization");

        let later_source = AuthenticatedLocalLibraryBytes::new(
            crate::portable_path::PortableRelativePath::new(
                "org/example/gated/1/gated-later-1.jar",
            )
            .expect("later library source path"),
            LibraryComponentSourceKind::Library,
            bytes.clone(),
            bytes.len() as u64,
            Sha1::digest(&bytes).into(),
        )
        .expect("later authenticated local source");
        let later_error = match context.retain_local_sources(vec![later_source]).await {
            Err(error) => error,
            Ok(_) => panic!("cancelled reconstruction context must reject later work"),
        };
        assert!(matches!(
            later_error,
            DownloadError::FileOperation(error)
                if error.kind() == std::io::ErrorKind::Interrupted
        ));
    }
}
