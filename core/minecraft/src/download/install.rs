use super::asset_source::{
    AssetSourcePool, AuthenticatedAssetCacheProofSet, RetainedAssetComponentSource,
    RetainedAssetSourceSet,
};
use super::assets::{
    ASSET_OBJECT_BASE_URL, AssetSourceAcquisitionRequest, RetainedAssetsAcquisition,
    abort_asset_download_pipeline, acquire_asset_sources_with_cache, await_asset_download_pipeline,
    recv_asset_progress, spawn_asset_download_pipeline,
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
use super::plan::TransferPlan;
#[cfg(test)]
use super::runtime::spawn_test_runtime_source_pipeline;
use super::runtime::{
    finish_runtime_pipeline_after_artifacts, recv_runtime_progress, spawn_runtime_ensure_pipeline,
};
use super::transfer::{
    AuthenticatedSelectedArtifactSource, AuthenticatedSelectedArtifactVersionBundleParts,
    SelectedArtifactSourceRequest, acquire_authenticated_selected_artifact_source,
};
use crate::artifact_path::validate_artifact_path_segment;
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
use crate::managed_component_cache::ManagedComponentExactCache;
#[cfg(test)]
use crate::managed_component_effects::ComponentExecutionFault;
#[cfg(test)]
use crate::managed_component_lifecycle::publish_managed_component_effect_with_execution_fault;
use crate::managed_component_lifecycle::{
    ManagedComponentLifecycleOutcome, publish_managed_component_effect,
};
use crate::managed_component_publication::ComponentRollbackEffect;
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{ManagedRootPublicationLease, run_publication_blocking};
use crate::manifest::{ManifestEntry, VersionManifest, fetch_fresh_install_version_manifest};
use crate::rules::{Environment, default_environment};
use crate::runtime::{ManagedRuntimeCache, RuntimeSourceReceipt, acquire_preferred_runtime_source};
#[cfg(test)]
use crate::runtime::{
    TestRuntimeSourceDescriptor, acquire_test_runtime_source, authenticated_test_runtime_source,
};
use crate::version_bundle_publication::{
    VersionBundleTransactionEffect, VersionBundleTransactionSettledOutcome, publish_version_bundle,
    settle_version_bundle_publication,
};
use futures_util::StreamExt;
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
}

enum DownloaderRoot {
    Managed {
        library_root: PathBuf,
        runtime_cache: ManagedRuntimeCache,
    },
    SourceOnly,
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
}

#[derive(Clone)]
enum ManagedReconstructionMode {
    ProofOnly,
    VersionBundle,
    Libraries(ManagedLibrariesReconstructionContext),
    Assets(ManagedAssetsReconstructionContext),
    WholeInstance {
        libraries: ManagedLibrariesReconstructionContext,
        assets: ManagedAssetsReconstructionContext,
    },
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
        Self {
            mode: ManagedReconstructionMode::ProofOnly,
        }
    }

    pub(crate) async fn bind_libraries(managed_root: ManagedDir) -> Result<Self, DownloadError> {
        Ok(Self {
            mode: ManagedReconstructionMode::Libraries(ManagedLibrariesReconstructionContext {
                source_pool: LibrarySourcePool::new()?,
                cache_admission: ExactLibraryCacheAdmission::bind_guarded(managed_root).await?,
                cache_proofs: Arc::new(std::sync::Mutex::new(
                    AuthenticatedLibraryCacheProofSet::default(),
                )),
            }),
        })
    }

    pub(crate) async fn bind_assets(managed_root: ManagedDir) -> Result<Self, DownloadError> {
        Ok(Self {
            mode: ManagedReconstructionMode::Assets(ManagedAssetsReconstructionContext {
                source_pool: AssetSourcePool::new()?,
                cache: ManagedComponentExactCache::bind_guarded(
                    managed_root,
                    ManagedComponentKind::Assets,
                )
                .await
                .map_err(|_| {
                    DownloadError::Integrity(
                        "managed Assets exact-cache admission failed".to_string(),
                    )
                })?,
                authority: Arc::new(std::sync::Mutex::new(None)),
            }),
        })
    }

    pub(crate) fn version_bundle() -> Self {
        Self {
            mode: ManagedReconstructionMode::VersionBundle,
        }
    }

    pub(crate) async fn bind_whole_instance(
        managed_root: ManagedDir,
    ) -> Result<Self, DownloadError> {
        let libraries = ManagedLibrariesReconstructionContext {
            source_pool: LibrarySourcePool::new()?,
            cache_admission: ExactLibraryCacheAdmission::bind_guarded(managed_root.clone()).await?,
            cache_proofs: Arc::new(std::sync::Mutex::new(
                AuthenticatedLibraryCacheProofSet::default(),
            )),
        };
        let assets = ManagedAssetsReconstructionContext {
            source_pool: AssetSourcePool::new()?,
            cache: ManagedComponentExactCache::bind_guarded(
                managed_root,
                ManagedComponentKind::Assets,
            )
            .await
            .map_err(|_| {
                DownloadError::Integrity("managed Assets exact-cache admission failed".to_string())
            })?,
            authority: Arc::new(std::sync::Mutex::new(None)),
        };
        Ok(Self {
            mode: ManagedReconstructionMode::WholeInstance { libraries, assets },
        })
    }

    fn retains_version_bundle_sources(&self) -> bool {
        matches!(
            self.mode,
            ManagedReconstructionMode::VersionBundle
                | ManagedReconstructionMode::WholeInstance { .. }
        )
    }

    pub(crate) fn retains_library_sources(&self) -> bool {
        matches!(
            self.mode,
            ManagedReconstructionMode::Libraries(_)
                | ManagedReconstructionMode::WholeInstance { .. }
        )
    }

    fn phase_library_source_pool(&self) -> Result<LibrarySourcePool, DownloadError> {
        match &self.mode {
            ManagedReconstructionMode::Libraries(libraries)
            | ManagedReconstructionMode::WholeInstance { libraries, .. } => {
                Ok(libraries.source_pool.clone())
            }
            ManagedReconstructionMode::ProofOnly
            | ManagedReconstructionMode::VersionBundle
            | ManagedReconstructionMode::Assets(_) => LibrarySourcePool::new(),
        }
    }

    async fn requires_retained_exact_source(
        &self,
        job: &super::libraries::DownloadJob,
    ) -> Result<bool, DownloadError> {
        let libraries = match &self.mode {
            ManagedReconstructionMode::Libraries(libraries)
            | ManagedReconstructionMode::WholeInstance { libraries, .. } => libraries,
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
            ManagedReconstructionMode::Libraries(libraries)
            | ManagedReconstructionMode::WholeInstance { libraries, .. } => libraries,
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
            ManagedReconstructionMode::Libraries(libraries)
            | ManagedReconstructionMode::WholeInstance { libraries, .. } => std::mem::take(
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
            ManagedReconstructionMode::Assets(assets)
            | ManagedReconstructionMode::WholeInstance { assets, .. } => {
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
            ManagedReconstructionMode::Assets(assets)
            | ManagedReconstructionMode::WholeInstance { assets, .. } => assets,
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

    pub(crate) fn take_whole_instance_authority(
        self,
    ) -> Result<
        (
            AuthenticatedLibraryCacheProofSet,
            RetainedAssetSourceSet,
            AuthenticatedAssetCacheProofSet,
        ),
        DownloadError,
    > {
        let ManagedReconstructionMode::WholeInstance { libraries, assets } = self.mode else {
            return Err(DownloadError::Integrity(
                "whole-instance reconstruction lost its combined authority".to_string(),
            ));
        };
        let library_cache_proofs = take_library_cache_proofs(libraries.cache_proofs)?;
        let (asset_sources, asset_cache_proofs) = take_retained_assets_authority(assets.authority)?;
        Ok((library_cache_proofs, asset_sources, asset_cache_proofs))
    }

    pub(crate) async fn retain_local_sources(
        &self,
        sources: Vec<AuthenticatedLocalLibraryBytes>,
    ) -> Result<RetainedLibrarySourceSet, DownloadError> {
        if sources.is_empty() {
            return Ok(RetainedLibrarySourceSet::new());
        }
        let libraries = match &self.mode {
            ManagedReconstructionMode::Libraries(libraries)
            | ManagedReconstructionMode::WholeInstance { libraries, .. } => libraries,
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

fn take_library_cache_proofs(
    cache_proofs: Arc<std::sync::Mutex<AuthenticatedLibraryCacheProofSet>>,
) -> Result<AuthenticatedLibraryCacheProofSet, DownloadError> {
    Arc::try_unwrap(cache_proofs)
        .map_err(|_| {
            DownloadError::Integrity(
                "Libraries reconstruction authority still has live borrowers".to_string(),
            )
        })?
        .into_inner()
        .map_err(|_| DownloadError::Integrity("Libraries authority was poisoned".to_string()))
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

    async fn reconstruct_version_inner(
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
                    plan.expect_contribution();
                    let RetainedAssetsAcquisition {
                        asset_index_source: retained_index,
                        sources,
                        cache_proofs,
                    } = acquire_asset_sources_with_cache(
                        AssetSourceAcquisitionRequest::new(
                            self.client.clone(),
                            Arc::clone(&self.asset_object_base_url),
                            version.asset_index.id.clone(),
                            source,
                            None,
                            &plan,
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
            let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
            let authenticated = self
                .acquire_vanilla_plan(version_id, &version_manifest_entry, fact_tx.as_ref())
                .await?;
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
        send(progress(
            "version_json",
            0,
            1,
            Some(format!("{version_id}.json")),
        ));

        let AuthenticatedVanillaPlan {
            version,
            environment,
            pending_library_declarations,
            library_jobs,
            version_json_source,
            asset_index_source,
            runtime_source,
        } = authenticated;
        let library_cache_admission = ExactLibraryCacheAdmission::bind(self.managed_root()).await?;
        send(progress(
            "version_json",
            1,
            1,
            Some(format!("{version_id}.json")),
        ));
        let asset_index_bytes = asset_index_source
            .as_ref()
            .map(AuthenticatedSelectedArtifactSource::observed_size)
            .unwrap_or(0);
        if asset_index_source.is_some() {
            plan.contribute_total(asset_index_bytes);
            send(progress(
                "asset_index",
                0,
                1,
                Some(format!("{}.json", version.asset_index.id)),
            ));
        }
        if asset_index_source.is_some() {
            plan.add_done(asset_index_bytes);
            send(progress(
                "asset_index",
                1,
                1,
                Some(format!("{}.json", version.asset_index.id)),
            ));
        }
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
        let mut runtime_pipeline = if let Some(runtime_source) = runtime_source {
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

            let java_version = version.java_version.clone();
            Some(self.spawn_runtime_pipeline(java_version, runtime_source, plan.clone()))
        } else {
            None
        };

        let artifact_result = async {
            send(progress(
                "client_jar",
                0,
                1,
                Some(format!("{version_id}.jar")),
            ));
            let client = version.downloads.client.as_ref().ok_or_else(|| {
                DownloadError::ResolveManifest(
                    "authenticated version has no exact client artifact".to_string(),
                )
            })?;
            let http_client = self.client.clone();
            let url = client.url.clone();
            let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
            let max_bytes = exact_version_bundle_source_limit(client.size, "client")?;
            let logical_identity = version_id.to_string();
            let target = selected_download_source_label(
                SelectedDownloadArtifactKind::ClientJar,
                version_id,
            );
            let client_fact_tx = fact_tx.cloned();
            let client_jar_task = tokio::spawn(async move {
                acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                    client: &http_client,
                    kind: SelectedDownloadArtifactKind::ClientJar,
                    url: &url,
                    logical_identity: &logical_identity,
                    expected: &expected,
                    max_bytes,
                    target: &target,
                    fact_tx: client_fact_tx.as_ref(),
                })
                .await
            });
            let log_config_task = if let Some(logging) = version
                .logging
                .as_ref()
                .and_then(|logging| logging.client.as_ref())
            {
                send(progress("log_config", 0, 1, Some(logging.file.id.clone())));
                let http_client = self.client.clone();
                let file = logging.file.clone();
                let expected = ExpectedIntegrity::from_mojang(file.size, &file.sha1);
                let max_bytes = exact_version_bundle_source_limit(file.size, "log config")?;
                let target = selected_download_source_label(
                    SelectedDownloadArtifactKind::LogConfig,
                    &file.id,
                );
                let log_fact_tx = fact_tx.cloned();
                Some(tokio::spawn(async move {
                    acquire_authenticated_selected_artifact_source(
                        SelectedArtifactSourceRequest {
                            client: &http_client,
                            kind: SelectedDownloadArtifactKind::LogConfig,
                            url: &file.url,
                            logical_identity: &file.id,
                            expected: &expected,
                            max_bytes,
                            target: &target,
                            fact_tx: log_fact_tx.as_ref(),
                        },
                    )
                    .await
                }))
            } else {
                None
            };
            let mut asset_pipeline = asset_index_source.map(|source| {
                spawn_asset_download_pipeline(
                    self.managed_root().to_path_buf(),
                    self.client.clone(),
                    Arc::clone(&self.asset_object_base_url),
                    version.asset_index.id.clone(),
                    source,
                    fact_tx.cloned(),
                    plan.clone(),
                )
            });

            plan.contribute_total(
                library_jobs
                    .iter()
                    .map(|classified| classified.job().expected.size.unwrap_or(0))
                    .sum::<u64>(),
            );
            send(progress("libraries", 0, library_jobs.len() as i32, None));
            let client = self.client.clone();
            let source_pool = LibrarySourcePool::new()?;
            let total_library_jobs = library_jobs.len() as i32;
            let mut completed_library_jobs = 0;
            let library_result = async {
                let mut proofs = Vec::with_capacity(total_library_jobs as usize);
                let mut sources = Vec::with_capacity(total_library_jobs as usize);
                let mut library_downloads =
                    futures_util::stream::iter(library_jobs.into_iter().map(|classified| {
                        let client = client.clone();
                        let fact_tx = fact_tx.cloned();
                        let source_pool = source_pool.clone();
                        let cache_admission = library_cache_admission.clone();
                        async move {
                            acquire_retained_classified_library(
                                &client,
                                classified,
                                &cache_admission,
                                &source_pool,
                                fact_tx.as_ref(),
                            )
                            .await
                        }
                    }))
                    .buffer_unordered(library_download_concurrency());
                let mut asset_progress_open = asset_pipeline.is_some();
                let mut runtime_progress_open = runtime_pipeline.is_some();
                loop {
                    tokio::select! {
                        progress = recv_asset_progress(&mut asset_pipeline), if asset_progress_open => {
                            if let Some(progress) = progress {
                                send(progress);
                            } else {
                                asset_progress_open = false;
                            }
                        }
                        progress = recv_runtime_progress(&mut runtime_pipeline), if runtime_progress_open => {
                            if let Some(progress) = progress {
                                send(progress);
                            } else {
                                runtime_progress_open = false;
                            }
                        }
                        result = library_downloads.next() => {
                            let Some(result) = result else {
                                break;
                            };
                            let RetainedClassifiedLibraryAcquisition {
                                relative_path: _,
                                name,
                                observed_size,
                                proof,
                                source,
                            } = result?;
                            if let Some(proof) = proof {
                                proofs.push(proof);
                            }
                            if let Some(source) = source {
                                sources.push(source);
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
                    }
                }
                Ok::<_, DownloadError>((proofs, sources))
            }
            .await;
            let client_jar_result = await_selected_source_task(client_jar_task, "client").await;
            let log_config_result = match log_config_task {
                Some(task) => Some(await_selected_source_task(task, "log config").await),
                None => None,
            };
            if client_jar_result.is_ok() {
                plan.add_done(client_jar_bytes);
                send(progress(
                    "client_jar",
                    1,
                    1,
                    Some(format!("{version_id}.jar")),
                ));
            }
            if log_config_result.as_ref().is_some_and(Result::is_ok)
                && let Some(logging) = version
                    .logging
                    .as_ref()
                    .and_then(|logging| logging.client.as_ref())
            {
                plan.add_done(log_config_bytes);
                send(progress("log_config", 1, 1, Some(logging.file.id.clone())));
            }
            let asset_result = if client_jar_result.is_err()
                || log_config_result.as_ref().is_some_and(Result::is_err)
                || library_result.is_err()
            {
                abort_asset_download_pipeline(asset_pipeline).await;
                None
            } else {
                await_asset_download_pipeline(asset_pipeline, send).await?
            };
            let client_source = client_jar_result?;
            let log_config_source = log_config_result.transpose()?;
            let (library_proofs, library_sources) = library_result?;
            Ok::<_, DownloadError>((
                pending_library_declarations,
                library_proofs,
                library_sources,
                client_source,
                log_config_source,
                asset_result,
            ))
        }
        .await;

        let (
            runtime_receipt,
            (
                pending_library_declarations,
                library_proofs,
                library_sources,
                client_source,
                log_config_source,
                asset_result,
            ),
        ) = finish_runtime_pipeline_after_artifacts(runtime_pipeline, artifact_result, send)
            .await?;

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
    ) -> super::runtime::RuntimeEnsurePipeline {
        #[cfg(test)]
        if self.install_manifest.is_some() {
            return spawn_test_runtime_source_pipeline(source_receipt, plan);
        }
        spawn_runtime_ensure_pipeline(
            self.managed_runtime_cache().clone(),
            java_version,
            source_receipt,
            plan,
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
        || validate_artifact_path_segment(version_id).is_err()
        || validate_artifact_path_segment(&json_name).is_err()
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
        if validate_artifact_path_segment(&logging.file.id).is_err()
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
        || validate_artifact_path_segment(&asset_index.id).is_err()
        || validate_artifact_path_segment(&index_name).is_err()
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
        )
        .await
        {
            Ok(ManagedProjectionSequenceOutcome::Committed(_lease)) => {
                Ok(authority.seal_after_version_bundle_commit())
            }
            Ok(ManagedProjectionSequenceOutcome::RolledBack { effect, .. }) => Err(
                managed_projection_sequence_rolled_back_install_error(effect.component()),
            ),
            Err(error) => Err(managed_projection_sequence_install_error(error.component())),
        }
    });
    owner.await.map_err(managed_install_owner_error)?
}

pub(crate) trait ManagedProjectionAuthority {
    fn component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ()>;
}

impl ManagedProjectionAuthority for PendingKnownGoodInstallAuthority {
    fn component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ()> {
        PendingKnownGoodInstallAuthority::component_projection(self, component).map_err(|_| ())
    }
}

impl ManagedProjectionAuthority for KnownGoodReconstructionReceipt {
    fn component_projection(
        &self,
        component: ManagedKnownGoodComponent,
    ) -> Result<ManagedComponentProjection<'_>, ()> {
        KnownGoodReconstructionReceipt::component_projection(self, component).map_err(|_| ())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ManagedProjectionSequenceEffect {
    Assets(ComponentRollbackEffect),
    Libraries(ComponentRollbackEffect),
    VersionBundle(VersionBundleTransactionEffect),
}

impl ManagedProjectionSequenceEffect {
    pub(crate) fn component(self) -> ManagedKnownGoodComponent {
        match self {
            Self::Assets(_) => ManagedKnownGoodComponent::Assets,
            Self::Libraries(_) => ManagedKnownGoodComponent::Libraries,
            Self::VersionBundle(_) => ManagedKnownGoodComponent::VersionBundle,
        }
    }
}

pub(crate) enum ManagedProjectionSequenceOutcome {
    Committed(ManagedRootPublicationLease),
    RolledBack {
        lease: ManagedRootPublicationLease,
        effect: ManagedProjectionSequenceEffect,
    },
}

pub(crate) enum ManagedProjectionSequenceError {
    Projection(ManagedKnownGoodComponent),
    Component(ManagedKnownGoodComponent),
    VersionBundle,
}

#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) enum ManagedProjectionSequenceFault {
    Assets(ComponentExecutionFault),
    Libraries(ComponentExecutionFault),
}

impl ManagedProjectionSequenceError {
    pub(crate) fn component(&self) -> ManagedKnownGoodComponent {
        match self {
            Self::Projection(component) | Self::Component(component) => *component,
            Self::VersionBundle => ManagedKnownGoodComponent::VersionBundle,
        }
    }
}

pub(crate) async fn publish_managed_projection_sequence(
    lease: ManagedRootPublicationLease,
    authority: &impl ManagedProjectionAuthority,
    asset_sources: Vec<RetainedAssetComponentSource>,
    library_sources: Vec<RetainedLibraryComponentSource>,
    version_bundle_source: AuthenticatedVersionBundleSource,
) -> Result<ManagedProjectionSequenceOutcome, ManagedProjectionSequenceError> {
    publish_managed_projection_sequence_inner(
        lease,
        authority,
        asset_sources,
        library_sources,
        version_bundle_source,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
pub(crate) async fn publish_managed_projection_sequence_with_fault(
    lease: ManagedRootPublicationLease,
    authority: &impl ManagedProjectionAuthority,
    asset_sources: Vec<RetainedAssetComponentSource>,
    library_sources: Vec<RetainedLibraryComponentSource>,
    version_bundle_source: AuthenticatedVersionBundleSource,
    fault: ManagedProjectionSequenceFault,
) -> Result<ManagedProjectionSequenceOutcome, ManagedProjectionSequenceError> {
    publish_managed_projection_sequence_inner(
        lease,
        authority,
        asset_sources,
        library_sources,
        version_bundle_source,
        Some(fault),
    )
    .await
}

async fn publish_managed_projection_sequence_inner(
    lease: ManagedRootPublicationLease,
    authority: &impl ManagedProjectionAuthority,
    asset_sources: Vec<RetainedAssetComponentSource>,
    library_sources: Vec<RetainedLibraryComponentSource>,
    version_bundle_source: AuthenticatedVersionBundleSource,
    #[cfg(test)] mut fault: Option<ManagedProjectionSequenceFault>,
) -> Result<ManagedProjectionSequenceOutcome, ManagedProjectionSequenceError> {
    let assets = authority
        .component_projection(ManagedKnownGoodComponent::Assets)
        .map_err(|()| {
            ManagedProjectionSequenceError::Projection(ManagedKnownGoodComponent::Assets)
        })?;
    #[cfg(test)]
    let assets_fault = match fault {
        Some(ManagedProjectionSequenceFault::Assets(execution)) => {
            fault = None;
            Some(execution)
        }
        Some(ManagedProjectionSequenceFault::Libraries(_)) | None => None,
    };
    #[cfg(test)]
    let assets = match assets_fault {
        Some(execution) => {
            publish_managed_component_effect_with_execution_fault(
                lease,
                assets,
                ManagedComponentKind::Assets,
                asset_sources,
                execution,
            )
            .await
        }
        None => {
            publish_managed_component_effect(
                lease,
                assets,
                ManagedComponentKind::Assets,
                asset_sources,
            )
            .await
        }
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
        ManagedComponentLifecycleOutcome::RolledBack(receipt) => {
            let effect = receipt.rollback_effect();
            return Ok(ManagedProjectionSequenceOutcome::RolledBack {
                lease: receipt.into_lease(),
                effect: ManagedProjectionSequenceEffect::Assets(effect),
            });
        }
    };
    let libraries = authority
        .component_projection(ManagedKnownGoodComponent::Libraries)
        .map_err(|()| {
            ManagedProjectionSequenceError::Projection(ManagedKnownGoodComponent::Libraries)
        })?;
    #[cfg(test)]
    let libraries_fault = match fault {
        Some(ManagedProjectionSequenceFault::Libraries(execution)) => Some(execution),
        Some(ManagedProjectionSequenceFault::Assets(_)) | None => None,
    };
    #[cfg(test)]
    let libraries = match libraries_fault {
        Some(execution) => {
            publish_managed_component_effect_with_execution_fault(
                lease,
                libraries,
                ManagedComponentKind::Libraries,
                library_sources,
                execution,
            )
            .await
        }
        None => {
            publish_managed_component_effect(
                lease,
                libraries,
                ManagedComponentKind::Libraries,
                library_sources,
            )
            .await
        }
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
        ManagedComponentLifecycleOutcome::RolledBack(receipt) => {
            let effect = receipt.rollback_effect();
            return Ok(ManagedProjectionSequenceOutcome::RolledBack {
                lease: receipt.into_lease(),
                effect: ManagedProjectionSequenceEffect::Libraries(effect),
            });
        }
    };
    let publication = {
        let projection = authority
            .component_projection(ManagedKnownGoodComponent::VersionBundle)
            .map_err(|()| {
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
        VersionBundleTransactionSettledOutcome::RolledBack { lease, effect } => {
            Ok(ManagedProjectionSequenceOutcome::RolledBack {
                lease,
                effect: ManagedProjectionSequenceEffect::VersionBundle(effect),
            })
        }
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

async fn await_selected_source_task(
    task: tokio::task::JoinHandle<Result<AuthenticatedSelectedArtifactSource, DownloadError>>,
    label: &'static str,
) -> Result<AuthenticatedSelectedArtifactSource, DownloadError> {
    task.await
        .map_err(|error| selected_source_task_error(error, label))?
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
}
