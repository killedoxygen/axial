use super::asset_source::{
    AssetSourcePool, AuthenticatedAssetCacheProof, AuthenticatedAssetCacheProofSet,
    RetainedAssetSourceSet,
};
use super::client::asset_download_concurrency;
use super::facts::selected_download_source_label;
use super::libraries::decode_sha1;
use super::model::{
    DownloadError, DownloadProgress, ExecutionDownloadFact, ExpectedIntegrity,
    SelectedDownloadArtifactKind, progress,
};
use super::plan::{TransferPlan, TransferPlanContribution};
use super::transfer::{
    AuthenticatedSelectedArtifactSource, SelectedArtifactSourceRequest,
    acquire_authenticated_selected_artifact_source,
};
use crate::asset_index::{LegacyAssetLayout, LegacyAssets, parse_asset_index};
use crate::known_good::{MAX_TIER2_AGGREGATE_BYTES, MAX_TIER2_ARTIFACT_BYTES, MAX_TIER2_ENTRIES};
use crate::loaders::types::LoaderError;
use crate::managed_blocking::{ManagedBlockingTaskError, ManagedBlockingWorkers};
use crate::managed_component_cache::{ManagedComponentExactCache, ManagedComponentExactCacheError};
use crate::managed_component_table::ManagedComponentKind;
use crate::managed_fs::{ManagedDir, ManagedLibraryOperation};
use crate::portable_path::{PortableFileName, PortableRelativePath};
use axial_resource::PhysicalIoClass;
use futures_util::StreamExt;
use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use tokio::sync::mpsc;

pub(crate) const ASSET_OBJECT_BASE_URL: &str = "https://resources.download.minecraft.net";
const ASSET_TREE_IO_SCRATCH_BYTES: u64 = 64 * 1024;

pub(super) struct AssetDownloadPipeline {
    task: Option<tokio::task::JoinHandle<Result<RetainedAssetsAcquisition, DownloadError>>>,
    progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
    progress_open: bool,
}

impl AssetDownloadPipeline {
    pub(super) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }
}

impl Drop for AssetDownloadPipeline {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

pub(super) struct RetainedAssetsAcquisition {
    pub(super) asset_index_source: AuthenticatedSelectedArtifactSource,
    pub(super) sources: RetainedAssetSourceSet,
    pub(super) cache_proofs: AuthenticatedAssetCacheProofSet,
}

pub(super) struct PreparedAssetDownloadPipeline {
    source_pool: AssetSourcePool,
    cache: ManagedComponentExactCache,
}

pub(super) enum AssetDownloadPipelineEvent {
    Progress(DownloadProgress),
    Complete {
        result: Result<RetainedAssetsAcquisition, DownloadError>,
        final_progress: Vec<DownloadProgress>,
    },
}

pub(super) struct AssetSourceAcquisitionInputs<P> {
    pub(super) client: reqwest::Client,
    pub(super) asset_object_base_url: Arc<str>,
    pub(super) asset_index_id: String,
    pub(super) asset_index_source: AuthenticatedSelectedArtifactSource,
    pub(super) fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    pub(super) plan: P,
    pub(super) contribution: TransferPlanContribution,
}

pub(super) struct AssetSourceAcquisitionRequest<'a, F> {
    inputs: AssetSourceAcquisitionInputs<&'a TransferPlan>,
    send: F,
}

pub(super) struct GuardedAssetSourceAcquisitionRequest<'a, F> {
    request: AssetSourceAcquisitionRequest<'a, F>,
    source_pool: AssetSourcePool,
    cache: ManagedComponentExactCache,
}

impl<'a, F> AssetSourceAcquisitionRequest<'a, F> {
    pub(super) fn new(inputs: AssetSourceAcquisitionInputs<&'a TransferPlan>, send: F) -> Self {
        Self { inputs, send }
    }

    pub(super) fn bind(
        self,
        source_pool: AssetSourcePool,
        cache: ManagedComponentExactCache,
    ) -> GuardedAssetSourceAcquisitionRequest<'a, F> {
        GuardedAssetSourceAcquisitionRequest {
            request: self,
            source_pool,
            cache,
        }
    }
}

pub(super) async fn prepare_asset_download_pipeline(
    library_root: &crate::managed_fs::ManagedLibraryOperation,
    workers: ManagedBlockingWorkers,
) -> Result<PreparedAssetDownloadPipeline, DownloadError> {
    let source_pool = AssetSourcePool::new_with_workers(workers.clone())?;
    let cache = ManagedComponentExactCache::bind_with_workers(
        library_root,
        ManagedComponentKind::Assets,
        workers,
    )
    .await
    .map_err(asset_cache_error)?;
    Ok(PreparedAssetDownloadPipeline { source_pool, cache })
}

pub(super) fn spawn_asset_download_pipeline(
    prepared: PreparedAssetDownloadPipeline,
    inputs: AssetSourceAcquisitionInputs<Arc<TransferPlan>>,
) -> AssetDownloadPipeline {
    let PreparedAssetDownloadPipeline { source_pool, cache } = prepared;
    let AssetSourceAcquisitionInputs {
        client,
        asset_object_base_url,
        asset_index_id,
        asset_index_source,
        fact_tx,
        plan,
        contribution,
    } = inputs;
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        acquire_asset_sources_with_cache(
            AssetSourceAcquisitionRequest::new(
                AssetSourceAcquisitionInputs {
                    client,
                    asset_object_base_url,
                    asset_index_id,
                    asset_index_source,
                    fact_tx,
                    plan: &plan,
                    contribution,
                },
                |progress| {
                    let _ = progress_tx.send(progress);
                },
            )
            .bind(source_pool, cache),
        )
        .await
    });

    AssetDownloadPipeline {
        task: Some(task),
        progress_rx,
        progress_open: true,
    }
}

pub(super) async fn next_asset_download_pipeline_event(
    pipeline: &mut AssetDownloadPipeline,
) -> AssetDownloadPipelineEvent {
    loop {
        enum PipelineEvent {
            Progress(Option<DownloadProgress>),
            Complete(
                Result<Result<RetainedAssetsAcquisition, DownloadError>, tokio::task::JoinError>,
            ),
        }
        let event = if pipeline.progress_open {
            let task = pipeline
                .task
                .as_mut()
                .expect("live asset pipeline owns its task");
            tokio::select! {
                biased;
                result = task => PipelineEvent::Complete(result),
                progress = pipeline.progress_rx.recv() => PipelineEvent::Progress(progress),
            }
        } else {
            let result = pipeline
                .task
                .as_mut()
                .expect("live asset pipeline owns its task")
                .await;
            PipelineEvent::Complete(result)
        };
        match event {
            PipelineEvent::Progress(Some(progress)) => {
                return AssetDownloadPipelineEvent::Progress(progress);
            }
            PipelineEvent::Progress(None) => {
                pipeline.progress_open = false;
            }
            PipelineEvent::Complete(result) => {
                pipeline.task.take();
                let result = result
                    .map_err(|error| {
                        DownloadError::ResolveManifest(format!("asset download task {error}"))
                    })
                    .and_then(|result| result);
                let final_progress = if result.is_ok() {
                    std::iter::from_fn(|| pipeline.progress_rx.try_recv().ok()).collect()
                } else {
                    Vec::new()
                };
                return AssetDownloadPipelineEvent::Complete {
                    result,
                    final_progress,
                };
            }
        }
    }
}

pub(super) async fn abort_asset_download_pipeline(pipeline: &mut Option<AssetDownloadPipeline>) {
    let Some(pipeline) = pipeline.as_mut() else {
        return;
    };
    if let Some(task) = pipeline.task.as_mut() {
        task.abort();
    }
    if let Some(task) = pipeline.task.take() {
        let _ = task.await;
    }
    pipeline.progress_rx.close();
    pipeline.progress_open = false;
}

pub(super) async fn acquire_asset_sources_with_cache<F>(
    request: GuardedAssetSourceAcquisitionRequest<'_, F>,
) -> Result<RetainedAssetsAcquisition, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let GuardedAssetSourceAcquisitionRequest {
        request:
            AssetSourceAcquisitionRequest {
                inputs:
                    AssetSourceAcquisitionInputs {
                        client,
                        asset_object_base_url,
                        asset_index_id,
                        asset_index_source,
                        fact_tx,
                        plan,
                        contribution,
                    },
                mut send,
            },
        source_pool,
        cache,
    } = request;
    source_pool.ensure_active()?;
    if asset_index_source.kind() != SelectedDownloadArtifactKind::AssetIndex
        || asset_index_source.logical_identity() != asset_index_id
    {
        return Err(DownloadError::Integrity(
            "authenticated asset index identity is invalid".to_string(),
        ));
    }
    let index =
        parse_asset_index(asset_index_source.bytes()).map_err(DownloadError::ParseVersion)?;
    let jobs = unique_asset_object_jobs(
        asset_index_source.observed_size(),
        index
            .objects
            .values()
            .map(|object| (object.hash.as_str(), object.size)),
    )?;
    let index_path = PortableRelativePath::new(&format!("indexes/{asset_index_id}.json"))
        .map_err(|_| DownloadError::Integrity("asset index path is invalid".to_string()))?;
    let index_source = source_pool
        .retain_index(&asset_index_source, index_path)
        .await?;

    let object_bytes = jobs.iter().try_fold(0_u64, |total, job| {
        total.checked_add(job.expected_size).ok_or_else(|| {
            DownloadError::Integrity("asset object byte budget overflowed".to_string())
        })
    })?;
    contribution.resolve(object_bytes);
    send(progress("assets", 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut asset_downloads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let source_pool = source_pool.clone();
        let cache = cache.clone();
        let asset_object_base_url = Arc::clone(&asset_object_base_url);
        async move {
            source_pool.ensure_active()?;
            if cache
                .full_sha1(&job.relative_path, job.expected_size)
                .await
                .map_err(asset_cache_error)?
                == Some(job.expected_sha1)
            {
                return Ok::<_, DownloadError>((
                    job.expected_size,
                    None,
                    Some(AuthenticatedAssetCacheProof::new(
                        job.relative_path,
                        job.expected_size,
                        job.expected_sha1,
                    )),
                ));
            }
            let permit = source_pool.reserve(job.expected_size).await?;
            let url = format!("{asset_object_base_url}/{}/{}", &job.hash[..2], job.hash);
            let target = selected_download_source_label(
                SelectedDownloadArtifactKind::AssetObject,
                &job.hash,
            );
            let source =
                acquire_authenticated_selected_artifact_source(SelectedArtifactSourceRequest {
                    client: &client,
                    kind: SelectedDownloadArtifactKind::AssetObject,
                    url: &url,
                    logical_identity: &job.hash,
                    expected: &job.expected,
                    max_bytes: usize::try_from(job.expected_size).map_err(|_| {
                        DownloadError::Integrity(
                            "asset object size exceeds the platform bound".to_string(),
                        )
                    })?,
                    target: &target,
                    fact_tx: fact_tx.as_ref(),
                })
                .await?;
            let retained = source_pool
                .retain_object(&source, job.relative_path, permit)
                .await?;
            Ok((job.expected_size, Some(retained), None))
        }
    }))
    .buffer_unordered(asset_download_concurrency());
    let mut sources = RetainedAssetSourceSet::new();
    sources.insert(index_source)?;
    let mut cache_proofs = AuthenticatedAssetCacheProofSet::default();
    while let Some(result) = asset_downloads.next().await {
        let (bytes, source, cache_proof) = result?;
        if let Some(source) = source {
            sources.insert(source)?;
        }
        if let Some(cache_proof) = cache_proof {
            cache_proofs.insert(cache_proof)?;
        }
        plan.add_done(bytes);
        completed_jobs += 1;
        if completed_jobs == total_jobs || completed_jobs % 50 == 0 {
            send(progress("assets", completed_jobs, total_jobs, None));
        }
    }

    Ok(RetainedAssetsAcquisition {
        asset_index_source,
        sources,
        cache_proofs,
    })
}

/// Copies each verified object to `assets/virtual/<index id>/<name>`, replacing
/// stale or missing copies. The library owns this tree; nothing else writes it.
pub async fn materialize_virtual_assets(
    library: &ManagedLibraryOperation,
    asset_index_id: &str,
    assets: &LegacyAssets,
) -> Result<(), DownloadError> {
    if assets.layout != LegacyAssetLayout::Virtual {
        return Err(DownloadError::Integrity(
            "asset index does not name a virtual tree".to_string(),
        ));
    }
    let tree_name = PortableFileName::new_exact(asset_index_id)
        .map_err(|_| DownloadError::Integrity("asset index identity is invalid".to_string()))?;
    let mut jobs = HashMap::<&str, VirtualAssetJob>::new();
    for file in &assets.files {
        jobs.entry(&file.hash)
            .or_insert_with(|| VirtualAssetJob {
                hash: file.hash.clone(),
                size: file.size,
                sha1: file.sha1,
                destinations: Vec::new(),
            })
            .destinations
            .push(file.path.clone());
    }
    let workers = ManagedBlockingWorkers::new();
    let roots = {
        let library = library.clone();
        workers
            .run_physical(
                PhysicalIoClass::Heavy,
                ASSET_TREE_IO_SCRATCH_BYTES,
                move |_| {
                    let assets = library
                        .managed_directory()
                        .and_then(|root| root.open_child("assets"))
                        .map_err(managed_asset_error)?;
                    let objects = assets.open_child("objects").map_err(managed_asset_error)?;
                    let tree = assets
                        .open_or_create_child("virtual")
                        .and_then(|virtual_dir| {
                            virtual_dir.open_or_create_child(tree_name.as_str())
                        })
                        .map_err(managed_asset_error)?;
                    Ok::<_, DownloadError>((objects, tree))
                },
            )
            .await
            .map_err(asset_tree_worker_error)??
    };
    let mut copies = futures_util::stream::iter(jobs.into_values().map(|job| {
        let (objects, tree) = roots.clone();
        let workers = workers.clone();
        async move {
            workers
                .run_physical(
                    PhysicalIoClass::Heavy,
                    ASSET_TREE_IO_SCRATCH_BYTES,
                    move |_| materialize_virtual_asset(&objects, &tree, job),
                )
                .await
        }
    }))
    .buffer_unordered(asset_download_concurrency().clamp(1, 4));
    while let Some(result) = copies.next().await {
        result.map_err(asset_tree_worker_error)??;
    }
    Ok(())
}

struct VirtualAssetJob {
    hash: String,
    size: u64,
    sha1: [u8; 20],
    destinations: Vec<PortableRelativePath>,
}

fn materialize_virtual_asset(
    objects: &ManagedDir,
    tree: &ManagedDir,
    job: VirtualAssetJob,
) -> Result<(), DownloadError> {
    let prefix = asset_object_hash_prefix(&job.hash)?;
    let object_directory = objects.open_child(prefix).map_err(|error| match error {
        LoaderError::Io(error) if error.kind() == io::ErrorKind::NotFound => {
            DownloadError::Integrity("virtual asset source is missing".to_string())
        }
        error => managed_asset_error(error),
    })?;
    let source = object_directory
        .inspect_regular_file(&job.hash)
        .map_err(managed_asset_error)?
        .ok_or_else(|| DownloadError::Integrity("virtual asset source is missing".to_string()))?;
    if source.size() != job.size
        || object_directory
            .sha1_reverified_file_bytes(&job.hash, &source, job.size)
            .map_err(managed_asset_error)?
            != job.sha1
    {
        return Err(DownloadError::Integrity(
            "virtual asset source failed authentication".to_string(),
        ));
    }
    for destination in job.destinations {
        let (parent, name) = tree
            .open_or_create_relative_parent(&destination)
            .map_err(managed_asset_error)?;
        let current = parent
            .inspect_regular_file(&name)
            .map_err(managed_asset_error)?;
        let current = match current {
            Some(guard) if guard.size() == job.size => {
                parent
                    .sha1_reverified_file_bytes(&name, &guard, job.size)
                    .map_err(managed_asset_error)?
                    == job.sha1
            }
            Some(_) | None => false,
        };
        if !current {
            parent
                .copy_guarded_file_exact_authenticated(
                    &name,
                    &object_directory,
                    &job.hash,
                    &source,
                    job.sha1,
                )
                .map_err(managed_asset_error)?;
        }
    }
    Ok(())
}

fn managed_asset_error(error: LoaderError) -> DownloadError {
    match error {
        LoaderError::Io(error) => DownloadError::FileOperation(error),
        error => DownloadError::Integrity(error.to_string()),
    }
}

fn asset_tree_worker_error(error: ManagedBlockingTaskError) -> DownloadError {
    DownloadError::FileOperation(io::Error::other(format!(
        "virtual asset task failed: {error:?}"
    )))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AssetObjectDownloadJob {
    pub(super) hash: String,
    pub(super) relative_path: PortableRelativePath,
    pub(super) expected_size: u64,
    pub(super) expected_sha1: [u8; 20],
    pub(super) expected: ExpectedIntegrity,
}

pub(super) fn unique_asset_object_jobs<'a>(
    asset_index_size: u64,
    objects: impl IntoIterator<Item = (&'a str, i64)>,
) -> Result<Vec<AssetObjectDownloadJob>, DownloadError> {
    if asset_index_size > MAX_TIER2_ARTIFACT_BYTES {
        return Err(DownloadError::Integrity(
            "asset index exceeds the per-artifact bound".to_string(),
        ));
    }
    let mut jobs = Vec::new();
    let mut queued_hashes = HashMap::new();
    let mut aggregate_bytes = asset_index_size;

    for (hash, size) in objects {
        let hash = hash.to_ascii_lowercase();
        let prefix = asset_object_hash_prefix(&hash)?;
        let size = u64::try_from(size).map_err(|_| {
            DownloadError::Integrity("asset object has an invalid declared size".to_string())
        })?;
        if size > MAX_TIER2_ARTIFACT_BYTES {
            return Err(DownloadError::Integrity(
                "asset object exceeds the per-artifact bound".to_string(),
            ));
        }
        if let Some(previous_size) = queued_hashes.insert(hash.clone(), size) {
            if previous_size != size {
                return Err(DownloadError::Integrity(
                    "asset object digest has conflicting sizes".to_string(),
                ));
            }
            continue;
        }
        if jobs.len().saturating_add(1) >= MAX_TIER2_ENTRIES {
            return Err(DownloadError::Integrity(
                "asset inventory exceeds the entry bound".to_string(),
            ));
        }
        aggregate_bytes = aggregate_bytes.checked_add(size).ok_or_else(|| {
            DownloadError::Integrity("asset inventory byte budget overflowed".to_string())
        })?;
        if aggregate_bytes > MAX_TIER2_AGGREGATE_BYTES {
            return Err(DownloadError::Integrity(
                "asset inventory exceeds the aggregate byte bound".to_string(),
            ));
        }
        let expected_sha1 = decode_sha1(&hash).ok_or_else(|| {
            DownloadError::Integrity("asset object digest is invalid".to_string())
        })?;
        jobs.push(AssetObjectDownloadJob {
            relative_path: PortableRelativePath::new(&format!("objects/{prefix}/{hash}")).map_err(
                |_| DownloadError::Integrity("asset object path is invalid".to_string()),
            )?,
            expected_size: size,
            expected_sha1,
            expected: ExpectedIntegrity {
                size: Some(size),
                sha1: Some(hash.clone()),
            },
            hash,
        });
    }
    jobs.sort_by(|left, right| left.hash.cmp(&right.hash));
    Ok(jobs)
}

pub(super) fn asset_object_hash_prefix(hash: &str) -> Result<&str, DownloadError> {
    const SHA1_HEX_LEN: usize = 40;
    if hash.len() != SHA1_HEX_LEN {
        return Err(DownloadError::Integrity(format!(
            "malformed asset object hash: expected {SHA1_HEX_LEN} hex characters, got {}",
            hash.len()
        )));
    }
    if !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(DownloadError::Integrity(
            "malformed asset object hash: expected hex characters".to_string(),
        ));
    }
    Ok(&hash[..2])
}

pub(super) fn asset_cache_error(error: ManagedComponentExactCacheError) -> DownloadError {
    match error {
        ManagedComponentExactCacheError::Admission => {
            DownloadError::Integrity("asset cache admission failed".to_string())
        }
        ManagedComponentExactCacheError::Cancelled => DownloadError::FileOperation(io::Error::new(
            io::ErrorKind::Interrupted,
            "asset cache admission was cancelled",
        )),
        ManagedComponentExactCacheError::TaskStopped => DownloadError::FileOperation(
            io::Error::other("asset cache admission task stopped unexpectedly"),
        ),
    }
}
