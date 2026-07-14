use super::assets::{
    abort_asset_download_pipeline, await_asset_download_pipeline, recv_asset_progress,
    spawn_asset_download_pipeline,
};
use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::facts::selected_download_source_label;
use super::libraries::library_jobs_for;
use super::library_source::{
    LIBRARY_SOURCE_MAX_BYTES, LibrarySourcePool, LibrarySourceRequest,
    acquire_authenticated_library_source,
};
use super::model::{
    DownloadError, DownloadProgress, ExactLibraryDownloadProof, ExecutionDownloadFact,
    ExpectedIntegrity, SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind, progress,
};
use super::plan::TransferPlan;
#[cfg(test)]
use super::runtime::spawn_test_runtime_source_pipeline;
use super::runtime::{
    finish_runtime_pipeline_after_artifacts, recv_runtime_progress, spawn_runtime_ensure_pipeline,
};
use super::transfer::{
    AuthenticatedSelectedArtifactSource, MaterializedSelectedArtifactSource,
    SelectedArtifactSourceRequest, acquire_authenticated_selected_artifact_source,
    ensure_selected_artifact_with_client, materialize_authenticated_library_source,
    materialize_authenticated_selected_artifact_source, prepare_library_publication,
    prepare_selected_artifact_install,
};
use crate::artifact_path::validate_artifact_path_segment;
use crate::known_good::{
    KnownGoodArtifactKind, KnownGoodInstallReceipt, KnownGoodIntegrity,
    KnownGoodReconstructionReceipt, KnownGoodRoot, MAX_KNOWN_GOOD_ASSET_INDEX_BYTES,
    MAX_KNOWN_GOOD_VERSION_JSON_BYTES, MAX_TIER2_ARTIFACT_BYTES, ManagedComponentProjection,
    ManagedKnownGoodComponent, PendingVanillaInstallReceipt, authenticate_pending_vanilla_install,
    seal_completed_vanilla_install, seal_reconstructed_vanilla,
};
use crate::known_good_libraries::{
    ClassifiedLibraryDownload, LibraryAcquisition, PendingExactLibraryDeclarations,
    PendingStreamedLibraryDeclarations, SealedExactLibraryDeclarations,
    seal_vanilla_exact_library_declarations,
};
use crate::launch::{VersionJson, effective_java_version_for};
use crate::managed_fs::ManagedDir;
use crate::managed_publication::{ManagedRootPublicationLease, run_publication_blocking};
use crate::manifest::{ManifestEntry, VersionManifest, fetch_fresh_install_version_manifest};
use crate::paths::{assets_dir, libraries_dir, versions_dir};
use crate::rules::{Environment, default_environment};
use crate::runtime::{ManagedRuntimeCache, RuntimeSourceReceipt, acquire_preferred_runtime_source};
#[cfg(test)]
use crate::runtime::{
    TestRuntimeSourceDescriptor, acquire_test_runtime_source, authenticated_test_runtime_source,
};
use crate::version_bundle_publication::{
    ManagedVersionBundleCommitReceipt, ManagedVersionBundleFailureReceipt,
    ManagedVersionBundleRebuildError, publish_version_bundle,
};
use futures_util::StreamExt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs as async_fs;
use tokio::sync::mpsc;

pub struct Downloader {
    root: DownloaderRoot,
    client: reqwest::Client,
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
    version_json: AuthenticatedSelectedArtifactSource,
    client_jar: AuthenticatedSelectedArtifactSource,
    log_config: Option<AuthenticatedSelectedArtifactSource>,
}

impl AuthenticatedVersionBundleSource {
    #[cfg(test)]
    pub(crate) fn version_id(&self) -> &str {
        &self.version_id
    }

    pub(crate) fn matches_projection(&self, projection: &ManagedComponentProjection<'_>) -> bool {
        version_bundle_sources_match_projection(
            &self.version_id,
            projection,
            &self.version_json,
            &self.client_jar,
            self.log_config.as_ref(),
        )
    }

    pub(crate) fn into_sources(
        self,
    ) -> (
        AuthenticatedSelectedArtifactSource,
        AuthenticatedSelectedArtifactSource,
        Option<AuthenticatedSelectedArtifactSource>,
    ) {
        (self.version_json, self.client_jar, self.log_config)
    }
}

pub(crate) struct ReconstructedVanillaClientAuthority {
    receipt: KnownGoodReconstructionReceipt,
    client_source: AuthenticatedSelectedArtifactSource,
}

pub(crate) struct ReconstructedVanillaProcessorAuthority {
    pending: PendingReconstructedVanillaProcessorAuthority,
    client_source: AuthenticatedSelectedArtifactSource,
    runtime_source: RuntimeSourceReceipt,
}

pub(crate) struct PendingReconstructedVanillaProcessorAuthority {
    parts: VanillaAuthorityParts,
}

impl ReconstructedVanillaClientAuthority {
    pub(crate) fn consume_for_overlay(
        self,
    ) -> (
        KnownGoodReconstructionReceipt,
        AuthenticatedSelectedArtifactSource,
    ) {
        (self.receipt, self.client_source)
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
    ) -> Result<KnownGoodReconstructionReceipt, DownloadError> {
        if self.parts.runtime_source.is_some() {
            return Err(DownloadError::ResolveManifest(
                "processor runtime authority was already completed".to_string(),
            ));
        }
        self.parts.runtime_source = Some(runtime_source);
        seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(self.parts)).map_err(
            |error| {
                DownloadError::ResolveManifest(format!(
                    "reconstructed source inventory could not be derived: {error:?}"
                ))
            },
        )
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
}

struct PendingVanillaInstall {
    receipt: PendingVanillaInstallReceipt,
    destinations: AllVanillaDestinationsMaterialized,
}

pub(crate) struct CompletedVanillaInstallAuthority {
    receipt: PendingVanillaInstallReceipt,
    _destinations: AllVanillaDestinationsMaterialized,
}

pub(crate) struct PendingVanillaInstallSourceAuthority {
    parts: VanillaAuthorityParts,
}

struct AllVanillaDestinationsMaterialized {
    version_id: String,
    libraries: usize,
    marker_removed: bool,
}

impl ReconstructedVanillaAuthority {
    fn new(parts: VanillaAuthorityParts) -> Self {
        Self { parts }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        VersionJson,
        Environment,
        crate::known_good_libraries::SealedExactLibraryDeclarations,
        AuthenticatedSelectedArtifactSource,
        Option<AuthenticatedSelectedArtifactSource>,
        Option<RuntimeSourceReceipt>,
    ) {
        self.parts.into_parts()
    }
}

impl PendingVanillaInstall {
    fn validate_completion(&self, version_id: &str) -> Result<(), DownloadError> {
        if self.destinations.version_id != version_id
            || self.destinations.marker_removed
            || self.destinations.libraries > crate::known_good::MAX_KNOWN_GOOD_ENTRIES
        {
            return Err(DownloadError::ResolveManifest(
                "vanilla install completion identity mismatch".to_string(),
            ));
        }
        Ok(())
    }

    fn complete_after_marker_removal(mut self) -> CompletedVanillaInstallAuthority {
        self.destinations.marker_removed = true;
        CompletedVanillaInstallAuthority {
            receipt: self.receipt,
            _destinations: self.destinations,
        }
    }
}

impl CompletedVanillaInstallAuthority {
    pub(crate) fn into_pending_receipt(self) -> PendingVanillaInstallReceipt {
        self.receipt
    }
}

impl PendingVanillaInstallSourceAuthority {
    fn new(parts: VanillaAuthorityParts) -> Self {
        Self { parts }
    }

    pub(crate) fn into_parts(
        self,
    ) -> (
        VersionJson,
        Environment,
        crate::known_good_libraries::SealedExactLibraryDeclarations,
        AuthenticatedSelectedArtifactSource,
        Option<AuthenticatedSelectedArtifactSource>,
        Option<RuntimeSourceReceipt>,
    ) {
        self.parts.into_parts()
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
            #[cfg(test)]
            install_manifest: None,
            #[cfg(test)]
            runtime_source: None,
        }
    }

    #[cfg(test)]
    fn source_only_with_test_install_manifest(manifest: VersionManifest) -> Self {
        Self {
            root: DownloaderRoot::SourceOnly,
            client: standard_minecraft_download_client(),
            install_manifest: Some(manifest),
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

    fn managed_root(&self) -> &Path {
        let DownloaderRoot::Managed { library_root, .. } = &self.root else {
            unreachable!("source-only downloader cannot materialize an installation");
        };
        library_root
    }

    async fn managed_root_identity(&self) -> Option<crate::managed_fs::ManagedDirectoryIdentity> {
        let DownloaderRoot::Managed { library_root, .. } = &self.root else {
            return None;
        };
        let library_root = library_root.clone();
        run_publication_blocking(move || ManagedDir::open_root(&library_root)?.identity())
            .await
            .ok()?
            .ok()
    }

    pub async fn owns_managed_version_bundle_commit_receipt(
        &self,
        receipt: &ManagedVersionBundleCommitReceipt,
    ) -> bool {
        self.managed_root_identity()
            .await
            .is_some_and(|identity| receipt.matches_root_identity(identity))
    }

    pub async fn owns_managed_version_bundle_failure_receipt(
        &self,
        receipt: &ManagedVersionBundleFailureReceipt,
    ) -> bool {
        self.managed_root_identity()
            .await
            .is_some_and(|identity| receipt.matches_root_identity(identity))
    }

    pub async fn rebuild_managed_vanilla_version_bundle(
        &self,
        version_id: &str,
        projection: ManagedComponentProjection<'_>,
    ) -> Result<ManagedVersionBundleCommitReceipt, ManagedVersionBundleRebuildError> {
        if crate::loaders::api::is_reserved_installed_loader_id(version_id) {
            return Err(ManagedVersionBundleRebuildError::SourceUnavailable);
        }
        let DownloaderRoot::Managed { library_root, .. } = &self.root else {
            return Err(ManagedVersionBundleRebuildError::RootUnavailable);
        };
        let source = self
            .reconstruct_vanilla_version_bundle_source(version_id)
            .await
            .map_err(|_| ManagedVersionBundleRebuildError::SourceUnavailable)?;
        let library_root = library_root.clone();
        let root = run_publication_blocking(move || ManagedDir::open_root(&library_root))
            .await
            .map_err(|_| ManagedVersionBundleRebuildError::RootUnavailable)?
            .map_err(|_| ManagedVersionBundleRebuildError::RootUnavailable)?;
        let lease = ManagedRootPublicationLease::acquire(root)
            .await
            .map_err(|_| ManagedVersionBundleRebuildError::RootUnavailable)?;
        publish_version_bundle(lease, source, projection)
            .await
            .map_err(ManagedVersionBundleRebuildError::Publication)
    }

    fn managed_runtime_cache(&self) -> &ManagedRuntimeCache {
        let DownloaderRoot::Managed { runtime_cache, .. } = &self.root else {
            unreachable!("source-only downloader cannot materialize a runtime");
        };
        runtime_cache
    }

    fn library_plan_root(&self) -> &Path {
        // Source-only plans bind safe relative library identities without granting a managed root.
        match &self.root {
            DownloaderRoot::Managed { library_root, .. } => library_root,
            DownloaderRoot::SourceOnly => Path::new(""),
        }
    }

    pub async fn install_version<F>(
        &self,
        version_id: &str,
        mut send: F,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        Box::pin(self.install_version_with_fact_sender(version_id, &mut send, None, None)).await
    }

    pub(crate) async fn reconstruct_version(
        &self,
        version_id: &str,
    ) -> Result<KnownGoodReconstructionReceipt, DownloadError> {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        self.reconstruct_version_inner(version_id, version_manifest_entry)
            .await
    }

    async fn reconstruct_version_inner(
        &self,
        version_id: &str,
        version_manifest_entry: ManifestEntry,
    ) -> Result<KnownGoodReconstructionReceipt, DownloadError> {
        let authority = self
            .reconstruct_vanilla_authority(version_id, &version_manifest_entry)
            .await?;
        seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(authority)).map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "reconstructed source inventory could not be derived: {error:?}"
            ))
        })
    }

    pub(crate) async fn reconstruct_version_with_client_source(
        &self,
        version_id: &str,
    ) -> Result<ReconstructedVanillaClientAuthority, DownloadError> {
        let (authority, client_source) = self
            .reconstruct_version_with_client_authority(version_id)
            .await?;
        let receipt = seal_reconstructed_vanilla(ReconstructedVanillaAuthority::new(authority))
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "reconstructed source inventory could not be derived: {error:?}"
                ))
            })?;
        Ok(ReconstructedVanillaClientAuthority {
            receipt,
            client_source,
        })
    }

    pub(crate) async fn reconstruct_version_for_processor(
        &self,
        version_id: &str,
    ) -> Result<ReconstructedVanillaProcessorAuthority, DownloadError> {
        let (mut authority, client_source) = self
            .reconstruct_version_with_client_authority(version_id)
            .await?;
        let runtime_source = authority.runtime_source.take().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated base version has no processor runtime source".to_string(),
            )
        })?;
        Ok(ReconstructedVanillaProcessorAuthority {
            pending: PendingReconstructedVanillaProcessorAuthority { parts: authority },
            client_source,
            runtime_source,
        })
    }

    async fn reconstruct_vanilla_version_bundle_source(
        &self,
        version_id: &str,
    ) -> Result<AuthenticatedVersionBundleSource, DownloadError> {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        let AuthenticatedVanillaVersionSource {
            version,
            environment: _,
            version_json_source,
        } = self
            .acquire_vanilla_version_source(version_id, &version_manifest_entry, None)
            .await?;
        let client_jar = self
            .acquire_version_bundle_client_source(version_id, &version)
            .await?;
        let log_config = self
            .acquire_version_bundle_log_config_source(&version)
            .await?;

        Ok(AuthenticatedVersionBundleSource {
            version_id: version_id.to_string(),
            version_json: version_json_source,
            client_jar,
            log_config,
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

    async fn acquire_version_bundle_log_config_source(
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
    ) -> Result<(VanillaAuthorityParts, AuthenticatedSelectedArtifactSource), DownloadError> {
        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        let authority = self
            .reconstruct_vanilla_authority(version_id, &version_manifest_entry)
            .await?;
        let client_source = self
            .acquire_version_bundle_client_source(version_id, &authority.version)
            .await?;
        Ok((authority, client_source))
    }

    async fn reconstruct_vanilla_authority(
        &self,
        version_id: &str,
        version_manifest_entry: &ManifestEntry,
    ) -> Result<VanillaAuthorityParts, DownloadError> {
        let AuthenticatedVanillaPlan {
            version,
            environment,
            pending_library_declarations,
            library_jobs,
            version_json_source,
            asset_index_source,
            runtime_source,
        } = self
            .acquire_vanilla_plan(version_id, version_manifest_entry, None)
            .await?;
        let mut library_proofs = Vec::new();
        let source_pool = LibrarySourcePool::new();
        for classified in library_jobs {
            let (job, acquisition) = classified.into_parts();
            if acquisition == LibraryAcquisition::ExactDeclaration {
                continue;
            }
            let target = selected_download_source_label(
                SelectedDownloadArtifactKind::Library,
                job.relative_path.as_str(),
            );
            let source = acquire_authenticated_library_source(LibrarySourceRequest {
                client: &self.client,
                url: &job.url,
                expected: &job.expected,
                relative_path: &job.relative_path,
                max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                target: &target,
                pool: &source_pool,
                fact_tx: None,
            })
            .await?;
            library_proofs.push(source.into_exact_download_proof(job.is_native));
        }
        let library_declarations = pending_library_declarations
            .seal_streamed(library_proofs)
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "reconstructed library declarations could not be completed: {error:?}"
                ))
            })?;

        Ok(VanillaAuthorityParts {
            version,
            environment,
            libraries: library_declarations,
            version_source: version_json_source,
            asset_index_source,
            runtime_source,
        })
    }

    pub async fn install_version_with_facts_and_descriptors<F, G, H>(
        &self,
        version_id: &str,
        mut send: F,
        mut send_fact: G,
        mut send_descriptor: H,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
        G: FnMut(ExecutionDownloadFact),
        H: FnMut(SelectedDownloadArtifactDescriptor),
    {
        let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
        let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
        let result = Box::pin(self.install_version_with_fact_sender(
            version_id,
            &mut send,
            Some(fact_tx),
            Some(descriptor_tx),
        ))
        .await;
        while let Ok(fact) = fact_rx.try_recv() {
            send_fact(fact);
        }
        while let Ok(descriptor) = descriptor_rx.try_recv() {
            send_descriptor(descriptor);
        }
        result
    }

    async fn install_version_with_fact_sender<F>(
        &self,
        version_id: &str,
        send: &mut F,
        fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let version_dir = versions_dir(self.managed_root()).join(version_id);
        let marker_path = version_dir.join(".incomplete");
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
            async_fs::create_dir_all(&version_dir).await?;
            async_fs::write(&marker_path, b"installing").await?;
            let pending = self
                .install_version_inner(
                    version_id,
                    authenticated,
                    &mut send,
                    &plan,
                    fact_tx.as_ref(),
                    descriptor_tx.as_ref(),
                )
                .await?;
            pending.validate_completion(version_id)?;
            async_fs::remove_file(&marker_path).await?;
            let completed = pending.complete_after_marker_removal();
            Ok::<_, DownloadError>(seal_completed_vanilla_install(completed))
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
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<PendingVanillaInstall, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let version_dir = versions_dir(self.managed_root()).join(version_id);
        let json_path = version_dir.join(format!("{version_id}.json"));
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
        let version_json_install = prepare_selected_artifact_install(
            SelectedDownloadArtifactKind::VersionJson,
            &json_path,
            version_json_source.provider_url(),
            version_json_source.logical_identity(),
            version_json_source.expected(),
            fact_tx,
            descriptor_tx,
        )
        .await?;
        let version_json_source = materialize_authenticated_selected_artifact_source(
            version_json_install,
            version_json_source,
            fact_tx,
        )
        .await?;
        let selected_library_count = library_jobs.len();
        let asset_index_bytes = version
            .asset_index
            .size
            .try_into()
            .ok()
            .filter(|size: &u64| *size > 0)
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
        let asset_index_source = self
            .materialize_asset_index_source(&version, asset_index_source, fact_tx, descriptor_tx)
            .await?;
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
            let jar_path = version_dir.join(format!("{version_id}.jar"));
            let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
            let client_fact_tx = fact_tx.cloned();
            let client_descriptor_tx = descriptor_tx.cloned();
            let client_jar_task = tokio::spawn(async move {
                ensure_selected_artifact_with_client(
                    SelectedDownloadArtifactKind::ClientJar,
                    &http_client,
                    &url,
                    &jar_path,
                    &expected,
                    client_fact_tx.as_ref(),
                    client_descriptor_tx.as_ref(),
                )
                .await?;
                Ok::<(), DownloadError>(())
            });
            let mut asset_pipeline = asset_index_source.as_ref().map(|source| {
                spawn_asset_download_pipeline(
                    self.managed_root().to_path_buf(),
                    self.client.clone(),
                    source.shared_bytes(),
                    fact_tx.cloned(),
                    descriptor_tx.cloned(),
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
            let source_pool = LibrarySourcePool::new();
            let total_library_jobs = library_jobs.len() as i32;
            let mut completed_library_jobs = 0;
            let library_result = async {
                let mut proofs = Vec::with_capacity(total_library_jobs as usize);
                let mut library_downloads =
                    futures_util::stream::iter(library_jobs.into_iter().map(|classified| {
                        let (job, acquisition) = classified.into_parts();
                        let client = client.clone();
                        let fact_tx = fact_tx.cloned();
                        let descriptor_tx = descriptor_tx.cloned();
                        let source_pool = source_pool.clone();
                        async move {
                            if acquisition == LibraryAcquisition::FreshStream {
                                let target = selected_download_source_label(
                                    SelectedDownloadArtifactKind::Library,
                                    job.relative_path.as_str(),
                                );
                                let source = acquire_authenticated_library_source(
                                    LibrarySourceRequest {
                                        client: &client,
                                        url: &job.url,
                                        expected: &job.expected,
                                        relative_path: &job.relative_path,
                                        max_bytes: LIBRARY_SOURCE_MAX_BYTES,
                                        target: &target,
                                        pool: &source_pool,
                                        fact_tx: fact_tx.as_ref(),
                                    },
                                )
                                .await?;
                                let prepared = prepare_library_publication(
                                    self.managed_root(),
                                    job.relative_path.clone(),
                                    &job.url,
                                    &job.expected,
                                    job.is_native,
                                    fact_tx.as_ref(),
                                    descriptor_tx.as_ref(),
                                )
                                .await?;
                                let (identity, _) = materialize_authenticated_library_source(
                                    prepared,
                                    source,
                                    fact_tx.as_ref(),
                                )
                                .await?;
                                let (
                                    path,
                                    _destination,
                                    is_native,
                                    provider_url,
                                    expected,
                                    observed_size,
                                    sha1,
                                ) = identity.into_parts();
                                let proof = ExactLibraryDownloadProof::new(
                                    path,
                                    is_native,
                                    provider_url,
                                    expected,
                                    observed_size,
                                    sha1,
                                );
                                Ok::<_, DownloadError>((job.name, observed_size, Some(proof)))
                            } else {
                                ensure_selected_artifact_with_client(
                                    SelectedDownloadArtifactKind::Library,
                                    &client,
                                    &job.url,
                                    &job.path,
                                    &job.expected,
                                    fact_tx.as_ref(),
                                    descriptor_tx.as_ref(),
                                )
                                .await?;
                                Ok::<_, DownloadError>((
                                    job.name,
                                    job.expected.size.unwrap_or(0),
                                    None,
                                ))
                            }
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
                            let (name, bytes, proof) = result?;
                            if let Some(proof) = proof {
                                proofs.push(proof);
                            }
                            plan.add_done(bytes);
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
                Ok::<_, DownloadError>(proofs)
            }
            .await;
            let client_jar_result = await_client_jar_download(client_jar_task).await;
            if client_jar_result.is_ok() {
                plan.add_done(client_jar_bytes);
                send(progress(
                    "client_jar",
                    1,
                    1,
                    Some(format!("{version_id}.jar")),
                ));
            }
            if client_jar_result.is_err() || library_result.is_err() {
                abort_asset_download_pipeline(asset_pipeline).await;
            } else {
                await_asset_download_pipeline(asset_pipeline, send).await?;
            }
            client_jar_result?;
            let library_proofs = library_result?;

            if let Some(logging) = version
                .logging
                .as_ref()
                .and_then(|logging| logging.client.as_ref())
            {
                let log_config_path = assets_dir(self.managed_root())
                    .join("log_configs")
                    .join(&logging.file.id);
                send(progress("log_config", 0, 1, Some(logging.file.id.clone())));
                let expected =
                    ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1);
                ensure_selected_artifact_with_client(
                    SelectedDownloadArtifactKind::LogConfig,
                    &self.client,
                    &logging.file.url,
                    &log_config_path,
                    &expected,
                    fact_tx,
                    descriptor_tx,
                )
                .await?;
                plan.add_done(log_config_bytes);
            }
            Ok::<_, DownloadError>((pending_library_declarations, library_proofs))
        }
        .await;

        let (runtime_receipt, (pending_library_declarations, library_proofs)) =
            finish_runtime_pipeline_after_artifacts(runtime_pipeline, artifact_result, send)
                .await?;

        let library_declarations = pending_library_declarations
            .seal_streamed(library_proofs)
            .map_err(|error| {
                DownloadError::ResolveManifest(format!(
                    "installed library declarations could not be completed: {error:?}"
                ))
            })?;
        let receipt = authenticate_pending_vanilla_install(
            PendingVanillaInstallSourceAuthority::new(VanillaAuthorityParts {
                version,
                environment,
                libraries: library_declarations,
                version_source: version_json_source.into_authenticated_source(),
                asset_index_source: asset_index_source
                    .map(MaterializedSelectedArtifactSource::into_authenticated_source),
                runtime_source: runtime_receipt,
            }),
        )
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "installed source inventory could not be derived: {error:?}"
            ))
        })?;
        Ok(PendingVanillaInstall {
            receipt,
            destinations: AllVanillaDestinationsMaterialized {
                version_id: version_id.to_string(),
                libraries: selected_library_count,
                marker_removed: false,
            },
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
            .classify_jobs(
                &libraries_dir(self.library_plan_root()),
                library_jobs_for(self.library_plan_root(), &version.libraries, &environment)?,
            )
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
                    .map_err(|error| DownloadError::PrepareRuntime(error.to_string()))?,
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

    async fn materialize_asset_index_source(
        &self,
        version: &VersionJson,
        source: Option<AuthenticatedSelectedArtifactSource>,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<Option<MaterializedSelectedArtifactSource>, DownloadError> {
        let Some(source) = source else {
            return Ok(None);
        };
        let index_name = format!("{}.json", version.asset_index.id);
        let index_path = assets_dir(self.managed_root())
            .join("indexes")
            .join(index_name);
        let expected =
            ExpectedIntegrity::from_mojang(version.asset_index.size, &version.asset_index.sha1);
        let prepared = prepare_selected_artifact_install(
            SelectedDownloadArtifactKind::AssetIndex,
            &index_path,
            &version.asset_index.url,
            &version.asset_index.id,
            &expected,
            fact_tx,
            descriptor_tx,
        )
        .await?;
        materialize_authenticated_selected_artifact_source(prepared, source, fact_tx)
            .await
            .map(Some)
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
) -> Result<SealedExactLibraryDeclarations, DownloadError> {
    let jobs = {
        let (libraries, environment) = declarations.profile_plan_inputs().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "profile library reconstruction contract is missing".to_string(),
            )
        })?;
        library_jobs_for(Path::new(""), libraries, environment)?
    };
    let (pending, classified) = declarations
        .classify_jobs(&libraries_dir(Path::new("")), jobs)
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "profile library reconstruction classification failed: {error:?}"
            ))
        })?;
    let client = standard_minecraft_download_client();
    let source_pool = LibrarySourcePool::new();
    let mut proofs = Vec::new();
    for classified in classified {
        let (job, acquisition) = classified.into_parts();
        if acquisition == LibraryAcquisition::ExactDeclaration {
            continue;
        }
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::Library,
            job.relative_path.as_str(),
        );
        let source = acquire_authenticated_library_source(LibrarySourceRequest {
            client: &client,
            url: &job.url,
            expected: &job.expected,
            relative_path: &job.relative_path,
            max_bytes: LIBRARY_SOURCE_MAX_BYTES,
            target: &target,
            pool: &source_pool,
            fact_tx: None,
        })
        .await?;
        proofs.push(source.into_exact_download_proof(job.is_native));
    }
    pending.seal_streamed(proofs).map_err(|error| {
        DownloadError::ResolveManifest(format!(
            "profile library reconstruction could not be completed: {error:?}"
        ))
    })
}

pub(crate) async fn reconstruct_installer_library_declarations(
    sources: crate::loaders::PendingForgeReconstructionSources,
) -> Result<crate::loaders::BoundForgeInstallExecution, DownloadError> {
    reconstruct_installer_library_declarations_inner(sources, None).await
}

pub(crate) async fn reconstruct_installer_processor_sources(
    sources: crate::loaders::PendingForgeReconstructionSources,
    workspace: &crate::loaders::workspace::cleanup::ProcessorWorkspace,
) -> Result<crate::loaders::BoundForgeInstallExecution, DownloadError> {
    reconstruct_installer_library_declarations_inner(sources, Some(workspace)).await
}

async fn reconstruct_installer_library_declarations_inner(
    sources: crate::loaders::PendingForgeReconstructionSources,
    workspace: Option<&crate::loaders::workspace::cleanup::ProcessorWorkspace>,
) -> Result<crate::loaders::BoundForgeInstallExecution, DownloadError> {
    let (pending, jobs, mut required_execution_inputs) = sources.into_parts();
    if workspace.is_none() && !required_execution_inputs.is_empty() {
        return Err(DownloadError::ResolveManifest(
            "processor reconstruction sources require an ephemeral workspace".to_string(),
        ));
    }
    let client = standard_minecraft_download_client();
    let source_pool = LibrarySourcePool::new();
    let mut proofs = Vec::new();
    for classified in jobs {
        let (plan, acquisition) = classified.into_parts();
        let required_by_execution = required_execution_inputs.remove(&plan.relative_path);
        let stage_in_workspace = required_by_execution;
        if acquisition == LibraryAcquisition::ExactDeclaration && !stage_in_workspace {
            continue;
        }
        let target = selected_download_source_label(
            SelectedDownloadArtifactKind::Library,
            plan.relative_path.as_str(),
        );
        let max_bytes = if stage_in_workspace {
            128 << 20
        } else {
            LIBRARY_SOURCE_MAX_BYTES
        };
        let source = acquire_authenticated_library_source(LibrarySourceRequest {
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
        })
        .await?;
        if stage_in_workspace {
            let (file, path, size, sha1, expected, _target, provider_url, permit) =
                source.into_parts();
            workspace
                .ok_or_else(|| {
                    DownloadError::ResolveManifest(
                        "processor reconstruction workspace is missing".to_string(),
                    )
                })?
                .import_library_authenticated(&path, file, size, sha1)
                .await
                .map_err(|error| {
                    DownloadError::FileOperation(io::Error::other(error.to_string()))
                })?;
            drop(permit);
            if acquisition == LibraryAcquisition::FreshStream {
                proofs.push(ExactLibraryDownloadProof::new(
                    path,
                    plan.is_native,
                    provider_url,
                    expected,
                    size,
                    sha1,
                ));
            }
        } else {
            proofs.push(source.into_exact_download_proof(plan.is_native));
        }
    }
    if !required_execution_inputs.is_empty() {
        return Err(DownloadError::ResolveManifest(
            "processor reconstruction input source is missing".to_string(),
        ));
    }
    pending.complete_sources(proofs).map_err(|error| {
        DownloadError::ResolveManifest(format!(
            "installer library reconstruction could not be completed: {error}"
        ))
    })
}

fn exact_version_bundle_source_limit(size: i64, label: &str) -> Result<usize, DownloadError> {
    u64::try_from(size)
        .ok()
        .filter(|size| *size > 0 && *size <= MAX_TIER2_ARTIFACT_BYTES)
        .and_then(|size| usize::try_from(size).ok())
        .ok_or_else(|| {
            DownloadError::ResolveManifest(format!(
                "authenticated {label} exceeds the reconstruction source limit"
            ))
        })
}

fn version_bundle_sources_match_projection(
    version_id: &str,
    projection: &ManagedComponentProjection<'_>,
    version_json: &AuthenticatedSelectedArtifactSource,
    client_jar: &AuthenticatedSelectedArtifactSource,
    log_config: Option<&AuthenticatedSelectedArtifactSource>,
) -> bool {
    if projection.component() != ManagedKnownGoodComponent::VersionBundle
        || projection.entry_count() != 2 + usize::from(log_config.is_some())
    {
        return false;
    }
    let version_json_path = format!("{version_id}/{version_id}.json");
    let client_jar_path = format!("{version_id}/{version_id}.jar");
    if !source_matches_version_bundle_entry(
        projection,
        version_json,
        SelectedDownloadArtifactKind::VersionJson,
        KnownGoodArtifactKind::VersionMetadata,
        &KnownGoodRoot::Versions,
        &version_json_path,
    ) || !source_matches_version_bundle_entry(
        projection,
        client_jar,
        SelectedDownloadArtifactKind::ClientJar,
        KnownGoodArtifactKind::ClientJar,
        &KnownGoodRoot::Versions,
        &client_jar_path,
    ) {
        return false;
    }
    match log_config {
        Some(source) => source_matches_version_bundle_entry(
            projection,
            source,
            SelectedDownloadArtifactKind::LogConfig,
            KnownGoodArtifactKind::LogConfig,
            &KnownGoodRoot::Assets,
            &format!("log_configs/{}", source.logical_identity()),
        ),
        None => true,
    }
}

fn source_matches_version_bundle_entry(
    projection: &ManagedComponentProjection<'_>,
    source: &AuthenticatedSelectedArtifactSource,
    source_kind: SelectedDownloadArtifactKind,
    artifact_kind: KnownGoodArtifactKind,
    root: &KnownGoodRoot,
    path: &str,
) -> bool {
    if source.kind() != source_kind {
        return false;
    }
    let Some(projected) = projection.entries().iter().find(|projected| {
        let entry = projected.entry();
        entry.kind() == artifact_kind && entry.root() == root && entry.path().as_str() == path
    }) else {
        return false;
    };
    let (digest, size) = match projected.entry().integrity() {
        KnownGoodIntegrity::Sha1 { digest, size }
        | KnownGoodIntegrity::ExactBytes { digest, size } => (digest, *size),
        KnownGoodIntegrity::Directory | KnownGoodIntegrity::LinkTarget(_) => return false,
    };
    u64::try_from(source.bytes().len()).is_ok_and(|observed| observed == size)
        && source
            .expected()
            .size
            .is_none_or(|expected| expected == size)
        && source
            .expected()
            .sha1
            .as_deref()
            .is_some_and(|expected| expected.eq_ignore_ascii_case(digest.as_str()))
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

async fn await_client_jar_download(
    task: tokio::task::JoinHandle<Result<(), DownloadError>>,
) -> Result<(), DownloadError> {
    task.await.map_err(client_jar_task_error)?
}

fn client_jar_task_error(error: tokio::task::JoinError) -> DownloadError {
    let reason = if error.is_cancelled() {
        "cancelled"
    } else if error.is_panic() {
        "panicked"
    } else {
        "failed"
    };
    DownloadError::FileOperation(io::Error::other(format!(
        "client jar download task {reason}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use sha1::{Digest as _, Sha1};
    use std::collections::HashMap;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    struct VersionBundleServerFixture {
        manifest: VersionManifest,
        requests: mpsc::UnboundedReceiver<String>,
        version_json: Vec<u8>,
        client_jar: Vec<u8>,
        log_config: Option<Vec<u8>>,
    }

    async fn version_bundle_server(
        version_id: &str,
        with_log_config: bool,
    ) -> VersionBundleServerFixture {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind version bundle server");
        let base_url = format!("http://{}", listener.local_addr().expect("server address"));
        let client_jar = b"authenticated-client".to_vec();
        let log_config = with_log_config.then(|| b"<log4j/>".to_vec());
        let mut version = serde_json::json!({
            "id": version_id,
            "downloads": { "client": {
                "url": format!("{base_url}/client.jar"),
                "sha1": test_sha1(&client_jar),
                "size": client_jar.len()
            }},
            "assetIndex": {
                "id": "unreachable-assets",
                "url": "http://127.0.0.1:1/unreachable-assets.json",
                "sha1": "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                "size": 16,
                "totalSize": 16
            },
            "javaVersion": {
                "component": "java-runtime-delta",
                "majorVersion": 21
            },
            "libraries": [
                {
                    "name": "org.example:unreachable-exact:1.0.0",
                    "downloads": { "artifact": {
                        "path": "org/example/unreachable-exact/1.0.0/unreachable-exact-1.0.0.jar",
                        "url": "http://127.0.0.1:1/unreachable-exact.jar",
                        "sha1": "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                        "size": 16
                    }}
                },
                {
                    "name": "org.example:unreachable-fresh:1.0.0",
                    "url": "http://127.0.0.1:1/maven/",
                    "size": 16
                }
            ]
        });
        if let Some(bytes) = log_config.as_ref() {
            version["logging"] = serde_json::json!({
                "client": {
                    "argument": "-Dlog4j.configurationFile=${path}",
                    "file": {
                        "id": "client.xml",
                        "url": format!("{base_url}/log-config.xml"),
                        "sha1": test_sha1(bytes),
                        "size": bytes.len()
                    },
                    "type": "log4j2-xml"
                }
            });
        }
        let version_json = version.to_string().into_bytes();
        let manifest = serde_json::from_value(serde_json::json!({
            "latest": { "release": version_id, "snapshot": version_id },
            "versions": [{
                "id": version_id,
                "type": "release",
                "url": format!("{base_url}/version.json"),
                "sha1": test_sha1(&version_json),
                "complianceLevel": 1
            }]
        }))
        .expect("test manifest");
        let mut responses = HashMap::from([
            ("/version.json".to_string(), version_json.clone()),
            ("/client.jar".to_string(), client_jar.clone()),
        ]);
        if let Some(bytes) = log_config.as_ref() {
            responses.insert("/log-config.xml".to_string(), bytes.clone());
        }
        let responses = std::sync::Arc::new(responses);
        let (request_tx, requests) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let responses = std::sync::Arc::clone(&responses);
                let request_tx = request_tx.clone();
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut chunk = [0_u8; 1024];
                    loop {
                        let Ok(read) = socket.read(&mut chunk).await else {
                            return;
                        };
                        if read == 0 {
                            return;
                        }
                        request.extend_from_slice(&chunk[..read]);
                        if request.windows(4).any(|window| window == b"\r\n\r\n") {
                            break;
                        }
                    }
                    let Some(path) = String::from_utf8_lossy(&request)
                        .lines()
                        .next()
                        .and_then(|line| line.split_whitespace().nth(1))
                        .map(str::to_string)
                    else {
                        return;
                    };
                    let _ = request_tx.send(path.clone());
                    let (status, body) = responses
                        .get(&path)
                        .map(|body| ("200 OK", body.as_slice()))
                        .unwrap_or(("404 Not Found", b"not found"));
                    let headers = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = socket.write_all(headers.as_bytes()).await;
                    let _ = socket.write_all(body).await;
                });
            }
        });

        VersionBundleServerFixture {
            manifest,
            requests,
            version_json,
            client_jar,
            log_config,
        }
    }

    fn test_sha1(bytes: &[u8]) -> String {
        format!("{:x}", Sha1::digest(bytes))
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

    #[tokio::test]
    async fn version_bundle_source_ignores_unrelated_component_sources() {
        let mut fixture = version_bundle_server("bundle-source", true).await;
        let downloader = Downloader::source_only_with_test_install_manifest(fixture.manifest);
        assert!(matches!(&downloader.root, DownloaderRoot::SourceOnly));

        let source = downloader
            .reconstruct_vanilla_version_bundle_source("bundle-source")
            .await
            .expect("authenticated version bundle source");

        assert_eq!(source.version_id(), "bundle-source");
        let (version_json, client_jar, log_config) = source.into_sources();
        assert_eq!(
            version_json.kind(),
            SelectedDownloadArtifactKind::VersionJson
        );
        assert_eq!(version_json.logical_identity(), "bundle-source");
        assert_eq!(version_json.bytes(), fixture.version_json);
        assert_eq!(client_jar.kind(), SelectedDownloadArtifactKind::ClientJar);
        assert_eq!(client_jar.logical_identity(), "bundle-source");
        assert_eq!(client_jar.bytes(), fixture.client_jar);
        let log_config = log_config.expect("retained log config");
        assert_eq!(log_config.kind(), SelectedDownloadArtifactKind::LogConfig);
        assert_eq!(log_config.logical_identity(), "client.xml");
        assert_eq!(
            log_config.bytes(),
            fixture.log_config.as_deref().expect("fixture log config")
        );

        let mut requests =
            std::iter::from_fn(|| fixture.requests.try_recv().ok()).collect::<Vec<_>>();
        requests.sort();
        assert_eq!(
            requests,
            vec!["/client.jar", "/log-config.xml", "/version.json"]
        );
    }

    #[tokio::test]
    async fn version_bundle_source_omits_absent_log_config_without_a_request() {
        let mut fixture = version_bundle_server("bundle-without-log", false).await;
        let downloader = Downloader::source_only_with_test_install_manifest(fixture.manifest);

        let source = downloader
            .reconstruct_vanilla_version_bundle_source("bundle-without-log")
            .await
            .expect("authenticated version bundle source");

        let (_, _, log_config) = source.into_sources();
        assert!(log_config.is_none());
        let mut requests =
            std::iter::from_fn(|| fixture.requests.try_recv().ok()).collect::<Vec<_>>();
        requests.sort();
        assert_eq!(requests, vec!["/client.jar", "/version.json"]);
    }

    #[tokio::test]
    async fn loader_version_bundle_rebuild_is_refused_before_source_resolution() {
        let version_id = "loader-v2-invalid";
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id, b"{}", b"client", None,
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        let result = Downloader::source_only()
            .rebuild_managed_vanilla_version_bundle(version_id, projection)
            .await;
        assert!(matches!(
            result,
            Err(ManagedVersionBundleRebuildError::SourceUnavailable)
        ));
    }

    #[tokio::test]
    async fn version_bundle_publication_success_retains_exact_receipt_and_lane() {
        let version_id = "bundle-success";
        let fixture = version_bundle_server(version_id, false).await;
        let temporary = tempfile::TempDir::new().expect("version bundle root");
        let library_root = temporary.path().join("library");
        std::fs::create_dir(&library_root).expect("library root");
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id,
            &fixture.version_json,
            &fixture.client_jar,
            None,
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        let downloader = Downloader::with_test_install_manifest(&library_root, fixture.manifest);

        let receipt = downloader
            .rebuild_managed_vanilla_version_bundle(version_id, projection)
            .await
            .expect("version bundle publication");

        assert!(receipt.revalidate().await);
        assert!(
            downloader
                .owns_managed_version_bundle_commit_receipt(&receipt)
                .await
        );
        let fresh_projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("fresh version bundle projection");
        assert!(receipt.matches_projection(&fresh_projection));
        assert_eq!(
            receipt
                .dispositions()
                .iter()
                .copied()
                .map(|disposition| (
                    disposition.inventory_ordinal(),
                    disposition.disposition()
                ))
                .collect::<Vec<_>>(),
            vec![
                (
                    0,
                    crate::version_bundle_publication::ManagedVersionBundleDisposition::PublishedNew,
                ),
                (
                    1,
                    crate::version_bundle_publication::ManagedVersionBundleDisposition::PublishedNew,
                ),
            ]
        );
        let version_root = library_root.join("versions").join(version_id);
        assert_eq!(
            std::fs::read(version_root.join(format!("{version_id}.json")))
                .expect("published version json"),
            fixture.version_json
        );
        assert_eq!(
            std::fs::read(version_root.join(format!("{version_id}.jar")))
                .expect("published client jar"),
            fixture.client_jar
        );

        let publication_root = library_root.join(".axial-publication");
        let lane = publication_root.join("version-bundle");
        assert_eq!(
            directory_entry_names(&publication_root),
            vec!["publication.lock".to_string(), "version-bundle".to_string()]
        );
        assert_eq!(
            directory_entry_names(&lane),
            vec![
                "manifest.json".to_string(),
                "quarantine".to_string(),
                "staging".to_string(),
            ]
        );
        assert!(directory_entry_names(&lane.join("staging")).is_empty());
        assert!(directory_entry_names(&lane.join("quarantine")).is_empty());
    }

    fn directory_entry_names(path: &Path) -> Vec<String> {
        let mut entries = std::fs::read_dir(path)
            .expect("publication directory")
            .map(|entry| {
                entry
                    .expect("publication entry")
                    .file_name()
                    .into_string()
                    .expect("portable publication entry")
            })
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }

    #[tokio::test]
    async fn version_bundle_publication_rolls_back_in_reverse_after_an_effect_failure() {
        let version_id = "bundle-rollback";
        let fixture = version_bundle_server(version_id, false).await;
        let temporary = tempfile::TempDir::new().expect("version bundle root");
        let library_root = temporary.path().join("library");
        std::fs::create_dir(&library_root).expect("library root");
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id,
            &fixture.version_json,
            &fixture.client_jar,
            None,
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        crate::version_bundle_publication::fail_after_promotions_for_test(version_id, 1);
        let downloader = Downloader::with_test_install_manifest(&library_root, fixture.manifest);

        let error = downloader
            .rebuild_managed_vanilla_version_bundle(version_id, projection)
            .await
            .expect_err("injected promotion failure");
        let ManagedVersionBundleRebuildError::Publication(publication) = error else {
            panic!("effect failure classification");
        };
        assert_eq!(
            publication.failure_phase(),
            crate::version_bundle_publication::ManagedVersionBundleFailurePhase::Effect
        );
        let receipt = publication
            .into_effect_receipt()
            .expect("effect failure receipt");
        assert_eq!(
            receipt.effect(),
            crate::version_bundle_publication::ManagedVersionBundleEffect::Promotion
        );
        assert!(receipt.revalidate().await);
        assert!(
            downloader
                .owns_managed_version_bundle_failure_receipt(&receipt)
                .await
        );
        let fresh_projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("fresh version bundle projection");
        assert!(receipt.matches_projection(&fresh_projection));
        assert!(
            !library_root
                .join("versions")
                .join(version_id)
                .join(format!("{version_id}.json"))
                .exists()
        );
        assert!(
            !library_root
                .join("versions")
                .join(version_id)
                .join(format!("{version_id}.jar"))
                .exists()
        );
        assert!(
            library_root
                .join(".axial-publication/version-bundle/manifest.json")
                .is_file()
        );
    }

    #[tokio::test]
    async fn version_bundle_publication_restores_quarantined_replacements() {
        let version_id = "bundle-replacement-rollback";
        let fixture = version_bundle_server(version_id, false).await;
        let temporary = tempfile::TempDir::new().expect("version bundle root");
        let library_root = temporary.path().join("library");
        let version_root = library_root.join("versions").join(version_id);
        std::fs::create_dir_all(&version_root).expect("existing version root");
        let previous_json = b"previous-version-json";
        let previous_client = b"previous-client-jar";
        std::fs::write(
            version_root.join(format!("{version_id}.json")),
            previous_json,
        )
        .expect("previous version json");
        std::fs::write(
            version_root.join(format!("{version_id}.jar")),
            previous_client,
        )
        .expect("previous client jar");
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id,
            &fixture.version_json,
            &fixture.client_jar,
            None,
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        crate::version_bundle_publication::fail_after_promotions_for_test(version_id, 2);
        let downloader = Downloader::with_test_install_manifest(&library_root, fixture.manifest);

        let error = downloader
            .rebuild_managed_vanilla_version_bundle(version_id, projection)
            .await
            .expect_err("injected replacement failure");
        let ManagedVersionBundleRebuildError::Publication(publication) = error else {
            panic!("effect failure classification");
        };
        let receipt = publication
            .into_effect_receipt()
            .expect("effect failure receipt");
        assert_eq!(
            receipt.effect(),
            crate::version_bundle_publication::ManagedVersionBundleEffect::Promotion
        );
        assert!(receipt.revalidate().await);
        assert_eq!(
            std::fs::read(version_root.join(format!("{version_id}.json")))
                .expect("restored version json"),
            previous_json
        );
        assert_eq!(
            std::fs::read(version_root.join(format!("{version_id}.jar")))
                .expect("restored client jar"),
            previous_client
        );
    }

    #[tokio::test]
    async fn cancelling_version_bundle_caller_does_not_cancel_started_mutation() {
        let version_id = "bundle-cancellation";
        let fixture = version_bundle_server(version_id, false).await;
        let temporary = tempfile::TempDir::new().expect("version bundle root");
        let library_root = temporary.path().join("library");
        std::fs::create_dir(&library_root).expect("library root");
        let inventory = crate::known_good::KnownGoodInventory::version_bundle_for_test(
            version_id,
            &fixture.version_json,
            &fixture.client_jar,
            None,
        );
        let projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("version bundle projection");
        let (reached, release) =
            crate::version_bundle_publication::pause_after_promotions_for_test(version_id, 1);
        let downloader = Downloader::with_test_install_manifest(&library_root, fixture.manifest);

        {
            let publication =
                downloader.rebuild_managed_vanilla_version_bundle(version_id, projection);
            tokio::pin!(publication);
            tokio::select! {
                reached = reached => reached.expect("mutation reached first promotion"),
                result = &mut publication => panic!("publication completed before pause: {result:?}"),
            }
        }
        release.send(()).expect("release detached mutation");

        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            let version_directory = library_root.join("versions").join(version_id);
            loop {
                if version_directory
                    .join(format!("{version_id}.json"))
                    .is_file()
                    && version_directory
                        .join(format!("{version_id}.jar"))
                        .is_file()
                {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("detached mutation completed");
        assert_eq!(
            std::fs::read(
                library_root
                    .join("versions")
                    .join(version_id)
                    .join(format!("{version_id}.json"))
            )
            .expect("published version json"),
            fixture.version_json
        );
        assert_eq!(
            std::fs::read(
                library_root
                    .join("versions")
                    .join(version_id)
                    .join(format!("{version_id}.jar"))
            )
            .expect("published client jar"),
            fixture.client_jar
        );
        assert!(
            library_root
                .join(".axial-publication/version-bundle/manifest.json")
                .is_file()
        );

        let second_projection = inventory
            .managed_component_projection(ManagedKnownGoodComponent::VersionBundle)
            .expect("second version bundle projection");
        let second_error = downloader
            .rebuild_managed_vanilla_version_bundle(version_id, second_projection)
            .await
            .expect_err("retained lane must fail closed");
        let ManagedVersionBundleRebuildError::Publication(second_error) = second_error else {
            panic!("second attempt failure classification");
        };
        assert_eq!(
            second_error.failure_phase(),
            crate::version_bundle_publication::ManagedVersionBundleFailurePhase::PreEffect
        );
        assert!(matches!(
            second_error,
            crate::version_bundle_publication::ManagedVersionBundlePublicationError::LaneOccupied
        ));
    }
}
