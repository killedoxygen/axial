use super::client::asset_download_concurrency;
#[cfg(test)]
use super::integrity::existing_asset_object_satisfies;
use super::integrity::hash_file;
use super::model::{
    DownloadError, DownloadProgress, ExecutionDownloadFact, ExpectedIntegrity,
    SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind, progress,
};
use super::path_safety::{
    bounded_download_file_label, bounded_provider_path_label, filesystem_path, path_is_file,
};
use super::plan::TransferPlan;
use super::transfer::ensure_selected_artifact_with_client;
use crate::asset_index::AssetIndexFlags;
use crate::launch::AssetIndex as VersionAssetIndex;
use crate::paths::assets_dir;
use futures_util::StreamExt;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs as async_fs;
use tokio::sync::mpsc;

pub(super) struct AssetDownloadPipeline {
    task: tokio::task::JoinHandle<Result<(), DownloadError>>,
    progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
}

#[derive(Deserialize)]
struct AssetIndex {
    objects: HashMap<String, AssetObject>,
    #[serde(flatten)]
    flags: AssetIndexFlags,
}

#[derive(Deserialize)]
struct AssetObject {
    hash: String,
    #[serde(default)]
    size: i64,
}

pub(super) fn spawn_asset_download_pipeline(
    mc_dir: PathBuf,
    client: reqwest::Client,
    asset_index: VersionAssetIndex,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    plan: Arc<TransferPlan>,
) -> Option<AssetDownloadPipeline> {
    if asset_index.url.is_empty() {
        return None;
    }
    // Asset-object bytes are unknown until the index is parsed; reserve the
    // contribution so partial totals are not stamped as near-complete.
    plan.expect_contribution();

    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let asset_index_path = assets_dir(&mc_dir)
            .join("indexes")
            .join(format!("{}.json", asset_index.id));
        let _ = progress_tx.send(progress(
            "asset_index",
            0,
            1,
            Some(format!("{}.json", asset_index.id)),
        ));
        let expected = ExpectedIntegrity::from_mojang(asset_index.size, &asset_index.sha1);
        let index_bytes = expected.size.unwrap_or(0);
        plan.contribute_total(index_bytes);
        ensure_selected_artifact_with_client(
            SelectedDownloadArtifactKind::AssetIndex,
            &client,
            &asset_index.url,
            &asset_index_path,
            &expected,
            fact_tx.as_ref(),
            descriptor_tx.as_ref(),
        )
        .await?;
        plan.add_done(index_bytes);
        download_asset_objects_with_client(
            &mc_dir,
            client,
            &asset_index_path,
            fact_tx,
            descriptor_tx,
            &plan,
            |progress| {
                let _ = progress_tx.send(progress);
            },
        )
        .await
    });

    Some(AssetDownloadPipeline { task, progress_rx })
}

pub(super) async fn await_asset_download_pipeline<F>(
    pipeline: Option<AssetDownloadPipeline>,
    send: &mut F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let Some(AssetDownloadPipeline {
        mut task,
        mut progress_rx,
    }) = pipeline
    else {
        return Ok(());
    };

    loop {
        tokio::select! {
            progress = progress_rx.recv() => {
                if let Some(progress) = progress {
                    send(progress);
                }
            }
            result = &mut task => {
                while let Ok(progress) = progress_rx.try_recv() {
                    send(progress);
                }
                return result.map_err(|error| {
                    DownloadError::ResolveManifest(format!("asset download task {error}"))
                })?;
            }
        }
    }
}

pub(super) async fn abort_asset_download_pipeline(pipeline: Option<AssetDownloadPipeline>) {
    if let Some(AssetDownloadPipeline { task, .. }) = pipeline {
        task.abort();
        let _ = task.await;
    }
}

async fn download_asset_objects_with_client<F>(
    mc_dir: &Path,
    client: reqwest::Client,
    asset_index_path: &Path,
    fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
    descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    plan: &TransferPlan,
    mut send: F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let index = read_asset_index(asset_index_path).await?;
    let objects_dir = assets_dir(mc_dir).join("objects");
    let jobs = unique_asset_object_jobs(
        &objects_dir,
        index
            .objects
            .values()
            .map(|object| (object.hash.as_str(), object.size)),
    )?;

    plan.resolve_contribution(
        jobs.iter()
            .map(|job| job.expected.size.unwrap_or(0))
            .sum::<u64>(),
    );
    send(progress("assets", 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut asset_downloads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = client.clone();
        let fact_tx = fact_tx.clone();
        let descriptor_tx = descriptor_tx.clone();
        async move {
            let hash = job.hash;
            let path = job.path;
            let expected = job.expected;
            let bytes = expected.size.unwrap_or(0);
            let url = format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[..2],
                hash
            );
            ensure_selected_artifact_with_client(
                SelectedDownloadArtifactKind::AssetObject,
                &client,
                &url,
                &path,
                &expected,
                fact_tx.as_ref(),
                descriptor_tx.as_ref(),
            )
            .await?;
            Ok::<u64, DownloadError>(bytes)
        }
    }))
    .buffer_unordered(asset_download_concurrency());
    while let Some(result) = asset_downloads.next().await {
        let bytes = result?;
        plan.add_done(bytes);
        completed_jobs += 1;
        if completed_jobs == total_jobs || completed_jobs % 50 == 0 {
            send(progress("assets", completed_jobs, total_jobs, None));
        }
    }

    if index.flags.requires_virtual_repair() {
        let virtual_dir = assets_dir(mc_dir).join("virtual").join("legacy");
        copy_virtual_assets(
            &objects_dir,
            &virtual_dir,
            index
                .objects
                .into_iter()
                .map(|(name, object)| (name, object.hash)),
        )
        .await?;
    }

    Ok(())
}

pub async fn repair_virtual_assets_from_index(
    mc_dir: &Path,
    asset_index_path: &Path,
) -> Result<bool, DownloadError> {
    let index = read_asset_index(asset_index_path).await?;
    if !index.flags.requires_virtual_repair() {
        return Ok(false);
    }
    let objects_dir = assets_dir(mc_dir).join("objects");
    let virtual_dir = assets_dir(mc_dir).join("virtual").join("legacy");
    copy_virtual_assets(
        &objects_dir,
        &virtual_dir,
        index
            .objects
            .into_iter()
            .map(|(name, object)| (name, object.hash)),
    )
    .await?;
    Ok(true)
}

async fn read_asset_index(asset_index_path: &Path) -> Result<AssetIndex, DownloadError> {
    serde_json::from_str::<AssetIndex>(
        &async_fs::read_to_string(filesystem_path(asset_index_path).as_ref()).await?,
    )
    .map_err(DownloadError::ParseVersion)
}

pub(super) async fn recv_asset_progress(
    pipeline: &mut Option<AssetDownloadPipeline>,
) -> Option<DownloadProgress> {
    pipeline.as_mut()?.progress_rx.recv().await
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct AssetObjectDownloadJob {
    pub(super) hash: String,
    pub(super) path: PathBuf,
    pub(super) expected: ExpectedIntegrity,
}

pub(super) fn unique_asset_object_jobs<'a>(
    objects_dir: &Path,
    objects: impl IntoIterator<Item = (&'a str, i64)>,
) -> Result<Vec<AssetObjectDownloadJob>, DownloadError> {
    let mut jobs = Vec::new();
    let mut queued_hashes = HashSet::new();

    for (hash, size) in objects {
        let prefix = asset_object_hash_prefix(hash)?;
        if !queued_hashes.insert(hash.to_string()) {
            continue;
        }
        jobs.push(AssetObjectDownloadJob {
            hash: hash.to_string(),
            path: objects_dir.join(prefix).join(hash),
            expected: ExpectedIntegrity::from_mojang(size, hash),
        });
    }

    Ok(jobs)
}

pub fn asset_object_hash_prefix(hash: &str) -> Result<&str, DownloadError> {
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

#[cfg(test)]
pub(super) async fn missing_asset_object_jobs(
    candidates: Vec<AssetObjectDownloadJob>,
) -> Result<Vec<AssetObjectDownloadJob>, DownloadError> {
    let mut missing = Vec::new();
    let mut checks = futures_util::stream::iter(candidates.into_iter().map(|job| async move {
        if existing_asset_object_satisfies(&job.path, &job.expected).await? {
            Ok::<Option<AssetObjectDownloadJob>, DownloadError>(None)
        } else {
            Ok::<Option<AssetObjectDownloadJob>, DownloadError>(Some(job))
        }
    }))
    .buffer_unordered(asset_download_concurrency());

    while let Some(result) = checks.next().await {
        if let Some(job) = result? {
            missing.push(job);
        }
    }

    Ok(missing)
}

pub(super) async fn copy_virtual_assets(
    objects_dir: &Path,
    virtual_dir: &Path,
    assets: impl IntoIterator<Item = (String, String)>,
) -> Result<(), DownloadError> {
    let mut copies = futures_util::stream::iter(assets.into_iter().map(|(name, hash)| {
        let objects_dir = objects_dir.to_path_buf();
        let virtual_dir = virtual_dir.to_path_buf();
        async move {
            let src = objects_dir
                .join(asset_object_hash_prefix(&hash)?)
                .join(&hash);
            let dst = virtual_asset_destination(&virtual_dir, &name)?;
            copy_virtual_asset_if_missing(&src, &dst).await
        }
    }))
    .buffer_unordered(asset_download_concurrency());

    while let Some(result) = copies.next().await {
        result?;
    }

    Ok(())
}

pub(super) async fn copy_virtual_asset_if_missing(
    src: &Path,
    dst: &Path,
) -> Result<(), DownloadError> {
    if !path_is_file(src).await {
        return Err(DownloadError::Integrity(format!(
            "virtual asset source is missing: {}",
            bounded_download_file_label(src)
        )));
    }
    if virtual_asset_matches_source(src, dst).await? {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        async_fs::create_dir_all(filesystem_path(parent).as_ref()).await?;
    }
    async_fs::copy(filesystem_path(src).as_ref(), filesystem_path(dst).as_ref()).await?;
    Ok(())
}

async fn virtual_asset_matches_source(src: &Path, dst: &Path) -> Result<bool, DownloadError> {
    if !path_is_file(dst).await {
        return Ok(false);
    }
    let source = hash_file(src).await?;
    let destination = hash_file(dst).await?;
    Ok(source.size == destination.size && source.sha1 == destination.sha1)
}

pub fn virtual_asset_destination(root: &Path, asset_name: &str) -> Result<PathBuf, DownloadError> {
    if asset_name.trim().is_empty() {
        return Err(unsafe_virtual_asset_path_error(asset_name));
    }

    let mut destination = root.to_path_buf();
    for segment in asset_name.split(['/', '\\']) {
        if segment.is_empty()
            || segment.contains(':')
            || Path::new(segment)
                .components()
                .any(|component| !matches!(component, std::path::Component::Normal(_)))
        {
            return Err(unsafe_virtual_asset_path_error(asset_name));
        }
        destination.push(segment);
    }

    Ok(destination)
}

fn unsafe_virtual_asset_path_error(asset_name: &str) -> DownloadError {
    DownloadError::Integrity(format!(
        "unsafe virtual asset path: {}",
        bounded_provider_path_label(asset_name)
    ))
}
