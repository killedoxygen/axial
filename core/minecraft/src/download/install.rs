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
    KnownGoodInstallReceipt, KnownGoodReconstructionReceipt, MAX_KNOWN_GOOD_ASSET_INDEX_BYTES,
    MAX_KNOWN_GOOD_VERSION_JSON_BYTES, PendingVanillaInstallReceipt,
    authenticate_pending_vanilla_install, seal_completed_vanilla_install,
    seal_reconstructed_vanilla,
};
use crate::known_good_libraries::{
    ClassifiedLibraryDownload, LibraryAcquisition, PendingExactLibraryDeclarations,
    PendingStreamedLibraryDeclarations, SealedExactLibraryDeclarations,
    seal_vanilla_exact_library_declarations,
};
use crate::launch::{VersionJson, effective_java_version_for};
use crate::manifest::{ManifestEntry, VersionManifest, fetch_fresh_install_version_manifest};
use crate::paths::{assets_dir, libraries_dir, versions_dir};
use crate::rules::{Environment, default_environment};
use crate::runtime::{RuntimeSourceReceipt, acquire_preferred_runtime_source};
#[cfg(test)]
use crate::runtime::{
    TestRuntimeSourceDescriptor, acquire_test_runtime_source, authenticated_test_runtime_source,
};
use futures_util::StreamExt;
use std::io;
use std::path::Path;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs as async_fs;
use tokio::sync::mpsc;

pub struct Downloader {
    mc_dir: PathBuf,
    client: reqwest::Client,
    #[cfg(test)]
    install_manifest: Option<VersionManifest>,
    #[cfg(test)]
    runtime_source: Option<TestRuntimeSourceDescriptor>,
}

pub(crate) struct ReconstructedVanillaClientAuthority {
    receipt: KnownGoodReconstructionReceipt,
    client_source: AuthenticatedSelectedArtifactSource,
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

struct AuthenticatedVanillaPlan {
    version: VersionJson,
    environment: Environment,
    pending_library_declarations: PendingStreamedLibraryDeclarations,
    library_jobs: Vec<ClassifiedLibraryDownload>,
    version_json_source: AuthenticatedSelectedArtifactSource,
    asset_index_source: Option<AuthenticatedSelectedArtifactSource>,
    runtime_source: Option<RuntimeSourceReceipt>,
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
    pub fn new(mc_dir: impl Into<PathBuf>) -> Self {
        Self {
            mc_dir: mc_dir.into(),
            client: standard_minecraft_download_client(),
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
            mc_dir: mc_dir.into(),
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

    pub async fn reconstruct_version(
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
        const MAX_RECONSTRUCTED_CLIENT_BYTES: usize = 512 << 20;

        validate_install_version_id(version_id)?;
        let version_manifest_entry = self.resolve_manifest_entry(version_id).await?;
        let authority = self
            .reconstruct_vanilla_authority(version_id, &version_manifest_entry)
            .await?;
        let client = authority.version.downloads.client.as_ref().ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated version has no exact client artifact".to_string(),
            )
        })?;
        let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
        let expected_size = expected.size.ok_or_else(|| {
            DownloadError::ResolveManifest(
                "authenticated version has no exact client size".to_string(),
            )
        })?;
        let max_bytes = usize::try_from(expected_size)
            .ok()
            .filter(|size| *size <= MAX_RECONSTRUCTED_CLIENT_BYTES)
            .ok_or_else(|| {
                DownloadError::ResolveManifest(
                    "authenticated client exceeds the reconstruction source limit".to_string(),
                )
            })?;
        let target =
            selected_download_source_label(SelectedDownloadArtifactKind::ClientJar, version_id);
        let client_source =
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
        let version_dir = versions_dir(&self.mc_dir).join(version_id);
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
        let version_dir = versions_dir(&self.mc_dir).join(version_id);
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
                    self.mc_dir.clone(),
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
                                    &self.mc_dir,
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
                let log_config_path = assets_dir(&self.mc_dir)
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
        let expected = ExpectedIntegrity::from_sha1(&version_manifest_entry.sha1);
        let source_target =
            selected_download_source_label(SelectedDownloadArtifactKind::VersionJson, version_id);
        let source =
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
        let version = parse_vanilla_version_source(source.bytes(), version_id)?;
        validate_vanilla_exact_artifact_contracts(&version)?;
        let environment = default_environment();
        let declaration_source =
            seal_vanilla_exact_library_declarations(source, &version, &environment).map_err(
                |error| {
                    DownloadError::ResolveManifest(format!(
                        "authenticated library declarations could not be sealed: {error:?}"
                    ))
                },
            )?;
        let (library_declarations, version_json_source) = declaration_source.into_parts();
        let (pending_library_declarations, library_jobs) = library_declarations
            .classify_jobs(
                &libraries_dir(&self.mc_dir),
                library_jobs_for(&self.mc_dir, &version.libraries, &environment)?,
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
        spawn_runtime_ensure_pipeline(java_version, source_receipt, plan)
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
        let index_path = assets_dir(&self.mc_dir).join("indexes").join(index_name);
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

fn validate_vanilla_exact_artifact_contracts(version: &VersionJson) -> Result<(), DownloadError> {
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
            assert!(validate_vanilla_exact_artifact_contracts(&version).is_err());
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
            validate_vanilla_exact_artifact_contracts(&version).expect("absent logging");
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
            assert!(validate_vanilla_exact_artifact_contracts(&version).is_err());
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
}
