use crate::java::ensure_java_runtime;
use crate::launch::{
    AssetIndex as VersionAssetIndex, JavaVersion, Library, VersionJson, maven_to_path,
};
use crate::manifest::{ManifestEntry, fetch_version_manifest_cached};
use crate::paths::{assets_dir, libraries_dir, versions_dir};
use crate::rules::{current_os_arch, default_environment, evaluate_rules};
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use sha1::{Digest as _, Sha1};
use std::collections::HashSet;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::Duration;
use thiserror::Error;
use tokio::fs as async_fs;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;

const MIN_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 4;
const MAX_LIBRARY_DOWNLOAD_CONCURRENCY: usize = 16;
const LIBRARY_DOWNLOADS_PER_CORE: usize = 2;
const MIN_ASSET_DOWNLOAD_CONCURRENCY: usize = 8;
const MAX_ASSET_DOWNLOAD_CONCURRENCY: usize = 32;
const ASSET_DOWNLOADS_PER_CORE: usize = 4;
const DOWNLOAD_CLIENT_MAX_IDLE_PER_HOST: usize = MAX_ASSET_DOWNLOAD_CONCURRENCY;
const DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS: u64 = 120;
const DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS: u64 = 60;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadProgress {
    pub phase: String,
    pub current: i32,
    pub total: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub done: bool,
}

pub struct Downloader {
    mc_dir: PathBuf,
    client: reqwest::Client,
}

#[derive(Debug, Error)]
pub enum DownloadError {
    #[error("file operation failed: {0}")]
    FileOperation(#[from] io::Error),
    #[error("resolve manifest url: {0}")]
    ResolveManifest(String),
    #[error("request failed: {0}")]
    Request(#[from] reqwest::Error),
    #[error("parse version json: {0}")]
    ParseVersion(#[from] serde_json::Error),
    #[error("prepare java runtime: {0}")]
    PrepareRuntime(String),
    #[error("download integrity: {0}")]
    Integrity(String),
}

#[derive(Debug, Clone)]
struct DownloadJob {
    path: PathBuf,
    url: String,
    name: String,
    expected: ExpectedIntegrity,
}

#[derive(Debug, Clone)]
struct VersionJsonDownload {
    url: String,
    expected: ExpectedIntegrity,
    force_download: bool,
}

struct AssetDownloadPipeline {
    task: tokio::task::JoinHandle<Result<(), DownloadError>>,
    progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
struct ExpectedIntegrity {
    size: Option<u64>,
    sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ActualIntegrity {
    size: u64,
    sha1: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum DownloadIntegrityError {
    SizeMismatch {
        file: String,
        expected: u64,
        actual: u64,
    },
    Sha1Mismatch {
        file: String,
        expected: String,
        actual: String,
    },
    MissingSha1 {
        file: String,
    },
}

impl std::fmt::Display for DownloadIntegrityError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SizeMismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} size mismatch: expected {expected}, got {actual}"
            ),
            Self::Sha1Mismatch {
                file,
                expected,
                actual,
            } => write!(
                formatter,
                "{file} sha1 mismatch: expected {expected}, got {actual}"
            ),
            Self::MissingSha1 { file } => {
                write!(formatter, "{file} sha1 was not computed")
            }
        }
    }
}

impl ExpectedIntegrity {
    fn from_mojang(size: i64, sha1: &str) -> Self {
        Self {
            size: u64::try_from(size).ok().filter(|value| *value > 0),
            sha1: non_empty_sha1(sha1),
        }
    }

    fn from_sha1(sha1: &str) -> Self {
        Self {
            size: None,
            sha1: non_empty_sha1(sha1),
        }
    }

    fn has_evidence(&self) -> bool {
        self.size.is_some() || self.sha1.is_some()
    }
}

impl Downloader {
    pub fn new(mc_dir: impl Into<PathBuf>) -> Self {
        Self {
            mc_dir: mc_dir.into(),
            client: standard_minecraft_download_client(),
        }
    }

    pub async fn install_version<F>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        mut send: F,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        let version_dir = versions_dir(&self.mc_dir).join(version_id);
        let marker_path = version_dir.join(".incomplete");

        let install_result = async {
            async_fs::create_dir_all(&version_dir).await?;
            async_fs::write(&marker_path, b"installing").await?;
            self.install_version_inner(version_id, manifest_url, &mut send)
                .await
        }
        .await;

        match install_result {
            Ok(()) => {
                let _ = async_fs::remove_file(&marker_path).await;
                send(DownloadProgress {
                    phase: "done".to_string(),
                    current: 1,
                    total: 1,
                    file: None,
                    error: None,
                    done: true,
                });
                Ok(())
            }
            Err(error) => {
                send(DownloadProgress {
                    phase: "error".to_string(),
                    current: 0,
                    total: 0,
                    file: None,
                    error: Some(error.to_string()),
                    done: true,
                });
                Err(error)
            }
        }
    }

    async fn install_version_inner<F>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        send: &mut F,
    ) -> Result<(), DownloadError>
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

        let version_json_download =
            if let Some(url) = manifest_url.filter(|value| !value.trim().is_empty()) {
                VersionJsonDownload {
                    url: url.to_string(),
                    expected: ExpectedIntegrity::default(),
                    force_download: true,
                }
            } else {
                match self.resolve_manifest_download(version_id).await {
                    Ok(download) => download,
                    Err(error) if path_is_file(&json_path).await => VersionJsonDownload {
                        url: String::new(),
                        expected: ExpectedIntegrity::default(),
                        force_download: false,
                    },
                    Err(error) => return Err(error),
                }
            };
        let should_download_version_json = !version_json_download.url.is_empty()
            && (version_json_download.force_download
                || !existing_file_satisfies(&json_path, &version_json_download.expected).await?);
        if should_download_version_json {
            self.download_file(
                &version_json_download.url,
                &json_path,
                &version_json_download.expected,
            )
            .await?;
        }

        let version =
            serde_json::from_str::<VersionJson>(&async_fs::read_to_string(&json_path).await?)?;
        let runtime_task = if version.java_version.major_version > 0 {
            send(progress(
                "java_runtime",
                0,
                1,
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

            let mc_dir = self.mc_dir.clone();
            let java_version = version.java_version.clone();
            Some(tokio::spawn(async move {
                ensure_java_runtime(&mc_dir, &java_version, "")
                    .await
                    .map_err(|error| error.to_string())?;
                Ok::<_, String>(java_version)
            }))
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
            let client_jar_task = if let Some(client) = &version.downloads.client {
                let http_client = self.client.clone();
                let url = client.url.clone();
                let jar_path = version_dir.join(format!("{version_id}.jar"));
                let expected = ExpectedIntegrity::from_mojang(client.size, &client.sha1);
                Some(tokio::spawn(async move {
                    if !existing_file_satisfies(&jar_path, &expected).await? {
                        download_file_with_client(&http_client, &url, &jar_path, &expected).await?;
                    }
                    Ok::<(), DownloadError>(())
                }))
            } else {
                None
            };
            let mut asset_pipeline = spawn_asset_download_pipeline(
                self.mc_dir.clone(),
                self.client.clone(),
                version.asset_index.clone(),
            );

            let library_jobs = self.library_jobs(&version);
            send(progress("libraries", 0, library_jobs.len() as i32, None));
            let client = self.client.clone();
            let total_library_jobs = library_jobs.len() as i32;
            let mut completed_library_jobs = 0;
            let library_result = async {
                let mut library_downloads =
                    futures_util::stream::iter(library_jobs.into_iter().map(|job| {
                        let client = client.clone();
                        async move {
                            if !existing_file_satisfies(&job.path, &job.expected).await? {
                                download_file_with_client(
                                    &client,
                                    &job.url,
                                    &job.path,
                                    &job.expected,
                                )
                                .await?;
                            }
                            Ok::<String, DownloadError>(job.name)
                        }
                    }))
                    .buffer_unordered(library_download_concurrency());
                let mut asset_progress_open = asset_pipeline.is_some();
                loop {
                    if asset_progress_open {
                        let Some(asset_pipeline) = asset_pipeline.as_mut() else {
                            asset_progress_open = false;
                            continue;
                        };
                        let asset_progress_rx = &mut asset_pipeline.progress_rx;
                        tokio::select! {
                            progress = asset_progress_rx.recv() => {
                                if let Some(progress) = progress {
                                    send(progress);
                                } else {
                                    asset_progress_open = false;
                                }
                            }
                            result = library_downloads.next() => {
                                let Some(result) = result else {
                                    break;
                                };
                                let name = result?;
                                completed_library_jobs += 1;
                                send(progress(
                                    "libraries",
                                    completed_library_jobs,
                                    total_library_jobs,
                                    Some(name),
                                ));
                            }
                        }
                    } else {
                        let Some(result) = library_downloads.next().await else {
                            break;
                        };
                        let name = result?;
                        completed_library_jobs += 1;
                        send(progress(
                            "libraries",
                            completed_library_jobs,
                            total_library_jobs,
                            Some(name),
                        ));
                    }
                }
                Ok::<(), DownloadError>(())
            }
            .await;
            let client_jar_result = await_client_jar_download(client_jar_task).await;
            if client_jar_result.is_ok() && version.downloads.client.is_some() {
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
            library_result?;

            if let Some(logging) = version
                .logging
                .as_ref()
                .and_then(|logging| logging.client.as_ref())
                && !logging.file.url.is_empty()
            {
                let log_config_path = assets_dir(&self.mc_dir)
                    .join("log_configs")
                    .join(&logging.file.id);
                send(progress("log_config", 0, 1, Some(logging.file.id.clone())));
                let expected =
                    ExpectedIntegrity::from_mojang(logging.file.size, &logging.file.sha1);
                if !existing_file_satisfies(&log_config_path, &expected).await? {
                    self.download_file(&logging.file.url, &log_config_path, &expected)
                        .await?;
                }
            }
            Ok::<(), DownloadError>(())
        }
        .await;

        if let Some(java_version) =
            finish_runtime_task_after_artifacts(runtime_task, artifact_result).await?
        {
            send(progress(
                "java_runtime",
                1,
                1,
                Some(format!(
                    "Ready {} (Java {})",
                    if java_version.component.trim().is_empty() {
                        "managed runtime".to_string()
                    } else {
                        java_version.component.clone()
                    },
                    java_version.major_version
                )),
            ));
        }

        Ok(())
    }

    async fn resolve_manifest_download(
        &self,
        version_id: &str,
    ) -> Result<VersionJsonDownload, DownloadError> {
        let manifest = fetch_version_manifest_cached(&self.mc_dir)
            .await
            .map_err(|error| DownloadError::ResolveManifest(error.to_string()))?;
        manifest
            .versions
            .into_iter()
            .find(|entry| entry.id == version_id)
            .map(version_json_download_from_manifest_entry)
            .ok_or_else(|| {
                DownloadError::ResolveManifest(format!(
                    "version {version_id} not found in manifest"
                ))
            })
    }

    fn library_jobs(&self, version: &VersionJson) -> Vec<DownloadJob> {
        let env = default_environment();
        library_jobs_for(&self.mc_dir, &version.libraries, &env)
    }

    async fn download_file(
        &self,
        url: &str,
        destination: &Path,
        expected: &ExpectedIntegrity,
    ) -> Result<(), DownloadError> {
        if let Some(parent) = destination.parent() {
            async_fs::create_dir_all(parent).await?;
        }

        let tmp_path = destination.with_extension("tmp");
        let mut last_error: Option<DownloadError> = None;

        for attempt in 0..3 {
            let result = async {
                remove_stale_download_temp(&tmp_path).await?;
                let response = self.client.get(url).send().await?.error_for_status()?;
                let actual =
                    write_download_response(response, &tmp_path, destination, expected).await?;
                verify_downloaded_integrity(destination, expected, &actual)?;
                promote_download_temp(&tmp_path, destination).await?;
                Ok::<(), DownloadError>(())
            }
            .await;

            match result {
                Ok(()) => return Ok(()),
                Err(error) => {
                    last_error = Some(error);
                    let _ = remove_stale_download_temp(&tmp_path).await;
                    if attempt < 2 {
                        tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
                    }
                }
            }
        }

        Err(last_error
            .unwrap_or_else(|| DownloadError::ResolveManifest("download failed".to_string())))
    }
}

fn spawn_asset_download_pipeline(
    mc_dir: PathBuf,
    client: reqwest::Client,
    asset_index: VersionAssetIndex,
) -> Option<AssetDownloadPipeline> {
    if asset_index.url.is_empty() {
        return None;
    }

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
        if !existing_file_satisfies(&asset_index_path, &expected).await? {
            download_file_with_client(&client, &asset_index.url, &asset_index_path, &expected)
                .await?;
        }
        download_asset_objects_with_client(&mc_dir, client, &asset_index_path, |progress| {
            let _ = progress_tx.send(progress);
        })
        .await
    });

    Some(AssetDownloadPipeline { task, progress_rx })
}

async fn await_asset_download_pipeline<F>(
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

async fn abort_asset_download_pipeline(pipeline: Option<AssetDownloadPipeline>) {
    if let Some(AssetDownloadPipeline { task, .. }) = pipeline {
        task.abort();
        let _ = task.await;
    }
}

async fn download_asset_objects_with_client<F>(
    mc_dir: &Path,
    client: reqwest::Client,
    asset_index_path: &Path,
    mut send: F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    #[derive(Deserialize)]
    struct AssetIndex {
        objects: std::collections::HashMap<String, AssetObject>,
        #[serde(default, rename = "virtual")]
        virtual_flag: bool,
        #[serde(default, rename = "map_to_resources")]
        map_to_resources: bool,
    }

    #[derive(Deserialize)]
    struct AssetObject {
        hash: String,
        #[serde(default)]
        size: i64,
    }

    let index =
        serde_json::from_str::<AssetIndex>(&async_fs::read_to_string(asset_index_path).await?)?;
    let objects_dir = assets_dir(mc_dir).join("objects");
    let jobs = missing_asset_object_jobs(unique_asset_object_jobs(
        &objects_dir,
        index
            .objects
            .values()
            .map(|object| (object.hash.as_str(), object.size)),
    )?)
    .await?;

    send(progress("assets", 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut asset_downloads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = client.clone();
        async move {
            let hash = job.hash;
            let path = job.path;
            let expected = job.expected;
            let url = format!(
                "https://resources.download.minecraft.net/{}/{}",
                &hash[..2],
                hash
            );
            download_file_with_client(&client, &url, &path, &expected).await
        }
    }))
    .buffer_unordered(asset_download_concurrency());
    while let Some(result) = asset_downloads.next().await {
        result?;
        completed_jobs += 1;
        if completed_jobs == total_jobs || completed_jobs % 50 == 0 {
            send(progress("assets", completed_jobs, total_jobs, None));
        }
    }

    if index.virtual_flag || index.map_to_resources {
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

fn version_json_download_from_manifest_entry(entry: ManifestEntry) -> VersionJsonDownload {
    VersionJsonDownload {
        url: entry.url,
        expected: ExpectedIntegrity::from_sha1(&entry.sha1),
        force_download: false,
    }
}

pub async fn download_libraries<F>(
    mc_dir: &Path,
    libraries: &[Library],
    phase: &str,
    mut send: F,
) -> Result<(), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let client = standard_minecraft_download_client();
    let env = default_environment();
    let jobs = library_jobs_for(mc_dir, libraries, &env);

    send(progress(phase, 0, jobs.len() as i32, None));
    let total_jobs = jobs.len() as i32;
    let mut completed_jobs = 0;
    let mut downloads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = client.clone();
        async move {
            if !existing_file_satisfies(&job.path, &job.expected).await? {
                download_file_with_client(&client, &job.url, &job.path, &job.expected).await?;
            }
            Ok::<String, DownloadError>(job.name)
        }
    }))
    .buffer_unordered(library_download_concurrency());
    while let Some(result) = downloads.next().await {
        let name = result?;
        completed_jobs += 1;
        send(progress(phase, completed_jobs, total_jobs, Some(name)));
    }
    Ok(())
}

fn build_http_client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent("croopor/0.3")
        .timeout(timeout)
        .pool_max_idle_per_host(DOWNLOAD_CLIENT_MAX_IDLE_PER_HOST)
        .pool_idle_timeout(Duration::from_secs(DOWNLOAD_CLIENT_POOL_IDLE_TIMEOUT_SECS))
        .tcp_keepalive(Duration::from_secs(DOWNLOAD_CLIENT_TCP_KEEPALIVE_SECS))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

fn standard_minecraft_download_client() -> reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT
        .get_or_init(|| build_http_client(Duration::from_secs(300)))
        .clone()
}

fn library_download_concurrency() -> usize {
    adaptive_download_concurrency(
        available_parallelism(),
        MIN_LIBRARY_DOWNLOAD_CONCURRENCY,
        MAX_LIBRARY_DOWNLOAD_CONCURRENCY,
        LIBRARY_DOWNLOADS_PER_CORE,
    )
}

fn asset_download_concurrency() -> usize {
    adaptive_download_concurrency(
        available_parallelism(),
        MIN_ASSET_DOWNLOAD_CONCURRENCY,
        MAX_ASSET_DOWNLOAD_CONCURRENCY,
        ASSET_DOWNLOADS_PER_CORE,
    )
}

fn available_parallelism() -> usize {
    std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(MIN_LIBRARY_DOWNLOAD_CONCURRENCY)
}

fn adaptive_download_concurrency(
    cores: usize,
    minimum: usize,
    maximum: usize,
    per_core: usize,
) -> usize {
    cores
        .saturating_mul(per_core)
        .clamp(minimum, maximum.max(minimum))
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct AssetObjectDownloadJob {
    hash: String,
    path: PathBuf,
    expected: ExpectedIntegrity,
}

fn unique_asset_object_jobs<'a>(
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

fn asset_object_hash_prefix(hash: &str) -> Result<&str, DownloadError> {
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

async fn missing_asset_object_jobs(
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

async fn copy_virtual_assets(
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

fn progress(phase: &str, current: i32, total: i32, file: Option<String>) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file,
        error: None,
        done: false,
    }
}

async fn await_client_jar_download(
    task: Option<tokio::task::JoinHandle<Result<(), DownloadError>>>,
) -> Result<(), DownloadError> {
    let Some(task) = task else {
        return Ok(());
    };

    task.await.map_err(client_jar_task_error)??;
    Ok(())
}

async fn finish_runtime_task_after_artifacts(
    task: Option<tokio::task::JoinHandle<Result<JavaVersion, String>>>,
    artifact_result: Result<(), DownloadError>,
) -> Result<Option<JavaVersion>, DownloadError> {
    let Some(task) = task else {
        return artifact_result.map(|_| None);
    };

    let runtime_result = match task.await {
        Ok(result) => result.map_err(DownloadError::PrepareRuntime),
        Err(error) => Err(DownloadError::PrepareRuntime(error.to_string())),
    };

    match artifact_result {
        Ok(()) => runtime_result.map(Some),
        Err(error) => {
            let _ = runtime_result;
            Err(error)
        }
    }
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

fn resolve_library_download(lib: &Library, mc_dir: &Path) -> Option<DownloadJob> {
    let lib_dir = libraries_dir(mc_dir);
    if !lib.natives.is_empty()
        && lib
            .downloads
            .as_ref()
            .is_none_or(|downloads| downloads.artifact.is_none())
    {
        return None;
    }

    if let Some(artifact) = lib
        .downloads
        .as_ref()
        .and_then(|downloads| downloads.artifact.as_ref())
    {
        if !artifact.url.trim().is_empty() {
            let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
            return Some(DownloadJob {
                name: Path::new(&artifact.path)
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| lib.name.clone()),
                path,
                url: artifact.url.clone(),
                expected: ExpectedIntegrity::from_mojang(artifact.size, &artifact.sha1),
            });
        }
        return None;
    }

    let maven_path = maven_to_path(&lib.name);
    if maven_path.as_os_str().is_empty() {
        return None;
    }
    let base_url = if lib.url.is_empty() {
        "https://libraries.minecraft.net/".to_string()
    } else if lib.url.ends_with('/') {
        lib.url.clone()
    } else {
        format!("{}/", lib.url)
    };
    let path = lib_dir.join(&maven_path);
    Some(DownloadJob {
        name: path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| lib.name.clone()),
        path,
        url: format!(
            "{}{}",
            base_url,
            maven_path.to_string_lossy().replace('\\', "/")
        ),
        expected: ExpectedIntegrity::from_mojang(lib.size, &lib.sha1),
    })
}

fn resolve_native_download(lib: &Library, mc_dir: &Path, os_name: &str) -> Option<DownloadJob> {
    let lib_dir = libraries_dir(mc_dir);
    for classifier_key in native_classifier_candidates(lib, os_name) {
        if let Some(artifact) = lib
            .downloads
            .as_ref()
            .and_then(|downloads| downloads.classifiers.get(&classifier_key))
            && !artifact.url.trim().is_empty()
        {
            let path = resolve_path_under_root(&lib_dir, &artifact.path)?;
            return Some(DownloadJob {
                name: Path::new(&artifact.path)
                    .file_name()
                    .map(|value| value.to_string_lossy().to_string())
                    .unwrap_or_else(|| format!("{}:{classifier_key}", lib.name)),
                path,
                url: artifact.url.clone(),
                expected: ExpectedIntegrity::from_mojang(artifact.size, &artifact.sha1),
            });
        }
    }

    let classifier_key = native_classifier_candidates(lib, os_name)
        .into_iter()
        .next()?;
    let maven_path = maven_to_path(&format!("{}:{classifier_key}", lib.name));
    if maven_path.as_os_str().is_empty() {
        return None;
    }
    let base_url = if lib.url.is_empty() {
        "https://libraries.minecraft.net/".to_string()
    } else if lib.url.ends_with('/') {
        lib.url.clone()
    } else {
        format!("{}/", lib.url)
    };
    let path = lib_dir.join(&maven_path);
    Some(DownloadJob {
        name: path
            .file_name()
            .map(|value| value.to_string_lossy().to_string())
            .unwrap_or_else(|| format!("{}:{classifier_key}", lib.name)),
        path,
        url: format!(
            "{}{}",
            base_url,
            maven_path.to_string_lossy().replace('\\', "/")
        ),
        expected: ExpectedIntegrity::from_mojang(lib.size, &lib.sha1),
    })
}

fn native_classifier_candidates(lib: &Library, os_name: &str) -> Vec<String> {
    let Some(base) = lib.natives.get(os_name) else {
        return Vec::new();
    };

    let arch = current_os_arch();
    let mut candidates = Vec::new();
    let variants = match arch {
        "x86_64" => vec![
            base.replace("${arch}", "64"),
            base.replace("-${arch}", ""),
            base.replace("${arch}", "x86_64"),
        ],
        "x86" => vec![
            base.replace("${arch}", "32"),
            base.replace("${arch}", "x86"),
        ],
        "arm64" => vec![
            base.replace("${arch}", "arm64"),
            base.replace("${arch}", "64"),
        ],
        _ => vec![base.replace("${arch}", arch)],
    };

    for variant in variants {
        if !variant.is_empty() && !candidates.contains(&variant) {
            candidates.push(variant);
        }
    }

    candidates
}

fn library_jobs_for(
    mc_dir: &Path,
    libraries: &[Library],
    env: &crate::rules::Environment,
) -> Vec<DownloadJob> {
    let mut jobs = Vec::new();
    let mut queued_paths = HashSet::new();

    for lib in libraries {
        if !evaluate_rules(&lib.rules, env) {
            continue;
        }

        if crate::rules::is_native_library(&lib.name) && !native_name_matches_env(&lib.name, env) {
            continue;
        }

        if let Some(job) = resolve_library_download(lib, mc_dir)
            && queued_paths.insert(job.path.clone())
        {
            jobs.push(job);
        }
        if let Some(job) = resolve_native_download(lib, mc_dir, &env.os_name)
            && queued_paths.insert(job.path.clone())
        {
            jobs.push(job);
        }
    }

    jobs
}

fn native_name_matches_env(name: &str, env: &crate::rules::Environment) -> bool {
    let lower = name.to_ascii_lowercase();
    if !lower.contains("natives-") {
        return true;
    }
    if lower.contains("windows-arm64") {
        return env.os_name == "windows" && env.os_arch == "arm64";
    }
    if lower.contains("windows-x86") {
        return env.os_name == "windows" && env.os_arch == "x86";
    }
    if lower.contains("natives-windows") {
        return env.os_name == "windows" && env.os_arch == "x86_64";
    }
    if lower.contains("macos-arm64") || lower.contains("osx-arm64") {
        return env.os_name == "osx" && env.os_arch == "arm64";
    }
    if lower.contains("natives-macos") || lower.contains("natives-osx") {
        return env.os_name == "osx" && env.os_arch == "x86_64";
    }
    if lower.contains("linux-arm64") {
        return env.os_name == "linux" && env.os_arch == "arm64";
    }
    if lower.contains("linux-x86") {
        return env.os_name == "linux" && env.os_arch == "x86";
    }
    if lower.contains("natives-linux") {
        return env.os_name == "linux" && env.os_arch == "x86_64";
    }
    true
}

async fn download_file_with_client(
    client: &reqwest::Client,
    url: &str,
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<(), DownloadError> {
    if let Some(parent) = destination.parent() {
        async_fs::create_dir_all(parent).await?;
    }

    let tmp_path = destination.with_extension("tmp");
    let mut last_error: Option<DownloadError> = None;
    for attempt in 0..3 {
        let result = async {
            remove_stale_download_temp(&tmp_path).await?;
            let response = client.get(url).send().await?.error_for_status()?;
            let actual =
                write_download_response(response, &tmp_path, destination, expected).await?;
            verify_downloaded_integrity(destination, expected, &actual)?;
            promote_download_temp(&tmp_path, destination).await?;
            Ok::<(), DownloadError>(())
        }
        .await;

        match result {
            Ok(()) => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                let _ = remove_stale_download_temp(&tmp_path).await;
                if attempt < 2 {
                    tokio::time::sleep(Duration::from_millis(250 * (attempt + 1) as u64)).await;
                }
            }
        }
    }
    Err(last_error.unwrap_or_else(|| DownloadError::ResolveManifest("download failed".to_string())))
}

async fn write_download_response(
    response: reqwest::Response,
    tmp_path: &Path,
    destination: &Path,
    expected: &ExpectedIntegrity,
) -> Result<ActualIntegrity, DownloadError> {
    if let Some(expected_size) = expected.size
        && let Some(content_length) = response.content_length()
        && content_length > expected_size
    {
        return Err(download_size_mismatch(
            destination,
            expected_size,
            content_length,
        ));
    }

    let mut output = tokio::fs::File::create(tmp_path).await?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha1::new();
    let mut written = 0_u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        let next_written = written.saturating_add(chunk.len() as u64);
        if let Some(expected_size) = expected.size
            && next_written > expected_size
        {
            return Err(download_size_mismatch(
                destination,
                expected_size,
                next_written,
            ));
        }
        hasher.update(&chunk);
        output.write_all(&chunk).await?;
        written = next_written;
    }
    output.flush().await?;
    Ok(ActualIntegrity {
        size: written,
        sha1: Some(format!("{:x}", hasher.finalize())),
    })
}

async fn remove_stale_download_temp(temp_path: &Path) -> Result<(), DownloadError> {
    let metadata = match async_fs::symlink_metadata(temp_path).await {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(DownloadError::FileOperation(error)),
    };
    let file_type = metadata.file_type();
    let result = if metadata.is_dir() && !file_type.is_symlink() {
        async_fs::remove_dir_all(temp_path).await
    } else {
        async_fs::remove_file(temp_path).await
    };

    result.map_err(DownloadError::FileOperation)
}

async fn promote_download_temp(temp_path: &Path, destination: &Path) -> Result<(), DownloadError> {
    match async_fs::rename(temp_path, destination).await {
        Ok(()) => return Ok(()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(DownloadError::FileOperation(error));
        }
        Err(_) => {}
    }

    remove_existing_download_destination(destination).await;
    match async_fs::rename(temp_path, destination).await {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = async_fs::remove_file(temp_path).await;
            Err(DownloadError::FileOperation(error))
        }
    }
}

async fn remove_existing_download_destination(destination: &Path) {
    let Ok(metadata) = async_fs::symlink_metadata(destination).await else {
        return;
    };
    let file_type = metadata.file_type();
    if metadata.is_file() || file_type.is_symlink() {
        let _ = async_fs::remove_file(destination).await;
    }
}

async fn existing_file_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    let Ok(metadata) = async_fs::metadata(path).await else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    if !expected.has_evidence() {
        return Ok(true);
    }
    if let Some(expected_size) = expected.size
        && metadata.len() != expected_size
    {
        return Ok(false);
    }
    if expected.sha1.is_some() {
        let actual = hash_file(path).await?;
        return Ok(verify_download_integrity(path, expected, &actual).is_ok());
    }
    Ok(true)
}

async fn existing_asset_object_satisfies(
    path: &Path,
    expected: &ExpectedIntegrity,
) -> Result<bool, DownloadError> {
    let Ok(metadata) = async_fs::metadata(path).await else {
        return Ok(false);
    };
    if !metadata.is_file() {
        return Ok(false);
    }
    if let Some(expected_size) = expected.size {
        return Ok(metadata.len() == expected_size);
    }
    Ok(true)
}

fn verify_downloaded_integrity(
    label_path: &Path,
    expected: &ExpectedIntegrity,
    actual: &ActualIntegrity,
) -> Result<(), DownloadError> {
    if !expected.has_evidence() {
        return Ok(());
    }
    verify_download_integrity(label_path, expected, actual)
        .map_err(|error| DownloadError::Integrity(error.to_string()))?;
    Ok(())
}

async fn hash_file(path: &Path) -> Result<ActualIntegrity, DownloadError> {
    let mut file = async_fs::File::open(path).await?;
    let mut hasher = Sha1::new();
    let mut size = 0_u64;
    let mut buffer = vec![0_u8; 64 * 1024];

    loop {
        let read = file.read(&mut buffer).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        size += read as u64;
    }

    Ok(ActualIntegrity {
        size,
        sha1: Some(format!("{:x}", hasher.finalize())),
    })
}

fn verify_download_integrity(
    path: &Path,
    expected: &ExpectedIntegrity,
    actual: &ActualIntegrity,
) -> Result<(), DownloadIntegrityError> {
    let file = bounded_download_file_label(path);
    if let Some(expected_size) = expected.size
        && actual.size != expected_size
    {
        return Err(DownloadIntegrityError::SizeMismatch {
            file,
            expected: expected_size,
            actual: actual.size,
        });
    }
    if let Some(expected_sha1) = expected.sha1.as_deref() {
        let Some(actual_sha1) = actual.sha1.as_deref() else {
            return Err(DownloadIntegrityError::MissingSha1 { file });
        };
        if !actual_sha1.eq_ignore_ascii_case(expected_sha1) {
            return Err(DownloadIntegrityError::Sha1Mismatch {
                file,
                expected: expected_sha1.to_string(),
                actual: actual_sha1.to_string(),
            });
        }
    }
    Ok(())
}

fn download_size_mismatch(path: &Path, expected: u64, actual: u64) -> DownloadError {
    DownloadError::Integrity(
        DownloadIntegrityError::SizeMismatch {
            file: bounded_download_file_label(path),
            expected,
            actual,
        }
        .to_string(),
    )
}

fn non_empty_sha1(value: &str) -> Option<String> {
    let trimmed = value.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn bounded_download_file_label(path: &Path) -> String {
    const MAX_LABEL_CHARS: usize = 120;
    let sanitized = path
        .file_name()
        .and_then(|value| value.to_str())
        .filter(|value| !value.is_empty())
        .unwrap_or("artifact")
        .replace(['\r', '\n'], "?");
    let mut chars = sanitized.chars();
    let label = chars.by_ref().take(MAX_LABEL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{label}...")
    } else {
        label
    }
}

fn bounded_provider_path_label(path: &str) -> String {
    const MAX_LABEL_CHARS: usize = 120;
    let sanitized = path.replace(['\r', '\n'], "?");
    let mut chars = sanitized.chars();
    let label = chars.by_ref().take(MAX_LABEL_CHARS).collect::<String>();
    if chars.next().is_some() {
        format!("{label}...")
    } else {
        label
    }
}

async fn path_is_file(path: &Path) -> bool {
    matches!(async_fs::metadata(path).await, Ok(metadata) if metadata.is_file())
}

async fn copy_virtual_asset_if_missing(src: &Path, dst: &Path) -> Result<(), DownloadError> {
    if path_is_file(dst).await || !path_is_file(src).await {
        return Ok(());
    }
    if let Some(parent) = dst.parent() {
        async_fs::create_dir_all(parent).await?;
    }
    async_fs::copy(src, dst).await?;
    Ok(())
}

fn virtual_asset_destination(root: &Path, asset_name: &str) -> Result<PathBuf, DownloadError> {
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

fn resolve_path_under_root(root: &Path, relative: &str) -> Option<PathBuf> {
    let clean = PathBuf::from(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    if clean.as_os_str().is_empty() || clean.is_absolute() {
        return None;
    }
    let joined = root.join(&clean);
    let relative_check = joined.strip_prefix(root).ok()?;
    if relative_check
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
    {
        return None;
    }
    Some(joined)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::launch::{Library, LibraryArtifact, LibraryDownload};
    use crate::rules::Environment;
    use std::collections::{HashMap, HashSet};
    use std::fs;
    use std::path::Path;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::{SystemTime, UNIX_EPOCH};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn install_version_emits_terminal_error_when_setup_fails() {
        let root = temp_dir("setup-failure");
        fs::create_dir_all(&root).expect("create root");
        fs::write(versions_dir(&root), b"not a directory").expect("write versions sentinel");

        let downloader = Downloader::new(&root);
        let mut events = Vec::new();
        let result = downloader
            .install_version("1.20.1", None, |progress| events.push(progress))
            .await;

        assert!(result.is_err());
        assert_eq!(events.len(), 1);
        let event = &events[0];
        assert_eq!(event.phase, "error");
        assert_eq!(event.current, 0);
        assert_eq!(event.total, 0);
        assert_eq!(event.file, None);
        assert!(event.error.is_some());
        assert!(event.done);

        let _ = fs::remove_file(root.join("versions"));
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn install_version_starts_asset_index_before_library_download_finishes() {
        let root = temp_dir("overlap-assets-libraries");
        let (version_url, mut requests, release_library) = spawn_overlapped_install_server().await;
        let downloader = Downloader::new(&root);
        let install = tokio::spawn(async move {
            downloader
                .install_version("overlap", Some(&version_url), |_| {})
                .await
        });

        let mut saw_asset_index = false;
        while !saw_asset_index {
            let path = timeout(Duration::from_secs(2), requests.recv())
                .await
                .expect("request should arrive before library release")
                .expect("request event");
            if path == "/asset-index.json" {
                saw_asset_index = true;
            }
        }

        release_library.send(()).expect("release library response");
        install
            .await
            .expect("install task should join")
            .expect("install should succeed");

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn runtime_task_is_joined_when_artifact_install_fails() {
        let joined = Arc::new(AtomicBool::new(false));
        let joined_in_task = Arc::clone(&joined);
        let (release_tx, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = release_rx.await;
            joined_in_task.store(true, Ordering::SeqCst);
            Ok(JavaVersion::default())
        });
        let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());
        let mut result = Box::pin(finish_runtime_task_after_artifacts(
            Some(task),
            Err(artifact_error),
        ));

        assert!(
            timeout(Duration::from_millis(25), &mut result)
                .await
                .is_err()
        );

        release_tx.send(()).expect("release runtime task");
        let result = result.await;

        assert!(matches!(
            result,
            Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
        ));
        assert!(joined.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn runtime_error_is_reported_when_artifact_install_succeeds() {
        let task = tokio::spawn(async { Err::<JavaVersion, String>("runtime failed".to_string()) });

        let result = finish_runtime_task_after_artifacts(Some(task), Ok(())).await;

        assert!(matches!(
            result,
            Err(DownloadError::PrepareRuntime(message)) if message == "runtime failed"
        ));
    }

    #[tokio::test]
    async fn artifact_error_is_preserved_when_runtime_also_fails() {
        let task = tokio::spawn(async { Err::<JavaVersion, String>("runtime failed".to_string()) });
        let artifact_error = DownloadError::ResolveManifest("artifact failed".to_string());

        let result = finish_runtime_task_after_artifacts(Some(task), Err(artifact_error)).await;

        assert!(matches!(
            result,
            Err(DownloadError::ResolveManifest(message)) if message == "artifact failed"
        ));
    }

    #[test]
    fn mixed_windows_native_libraries_only_download_matching_arch() {
        let env = Environment {
            os_name: "windows".to_string(),
            os_arch: "x86_64".to_string(),
            os_version: String::new(),
            features: HashMap::new(),
        };
        let mc_dir = Path::new("/tmp/croopor-test");
        let libraries = vec![
            native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-arm64"),
            native_library("org.lwjgl:lwjgl:3.3.3:natives-windows-x86"),
            native_library("org.lwjgl:lwjgl:3.3.3:natives-windows"),
        ];

        let jobs = library_jobs_for(mc_dir, &libraries, &env);
        let names = jobs.into_iter().map(|job| job.name).collect::<Vec<_>>();

        assert!(
            names
                .iter()
                .any(|name| name.contains("natives-windows.jar"))
        );
        assert!(!names.iter().any(|name| name.contains("arm64")));
        assert!(!names.iter().any(|name| name.contains("-x86.jar")));
    }

    #[test]
    fn legacy_native_classifier_prefers_windows_generic_classifier() {
        let mut natives = HashMap::new();
        natives.insert("windows".to_string(), "natives-windows-${arch}".to_string());

        let mut classifiers = HashMap::new();
        classifiers.insert(
            "natives-windows".to_string(),
            artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows.jar"),
        );
        classifiers.insert(
            "natives-windows-arm64".to_string(),
            artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows-arm64.jar"),
        );
        classifiers.insert(
            "natives-windows-x86".to_string(),
            artifact("org/lwjgl/lwjgl/3.3.3/lwjgl-3.3.3-natives-windows-x86.jar"),
        );

        let lib = Library {
            name: "org.lwjgl:lwjgl:3.3.3".to_string(),
            downloads: Some(LibraryDownload {
                artifact: None,
                classifiers,
            }),
            natives,
            ..Library::default()
        };

        let job = resolve_native_download(&lib, Path::new("/tmp/croopor-test"), "windows")
            .expect("native download");

        assert!(job.name.contains("natives-windows.jar"));
        assert!(!job.name.contains("arm64"));
        assert!(!job.name.contains("-x86.jar"));
    }

    #[test]
    fn adaptive_download_concurrency_scales_with_bounds() {
        assert_eq!(adaptive_download_concurrency(1, 4, 16, 2), 4);
        assert_eq!(adaptive_download_concurrency(4, 4, 16, 2), 8);
        assert_eq!(adaptive_download_concurrency(32, 4, 16, 2), 16);
        assert_eq!(adaptive_download_concurrency(0, 8, 32, 4), 8);
    }

    #[test]
    fn library_jobs_deduplicate_same_destination() {
        let env = Environment {
            os_name: "linux".to_string(),
            os_arch: "x86_64".to_string(),
            os_version: String::new(),
            features: HashMap::new(),
        };
        let mc_dir = Path::new("/tmp/croopor-test");
        let libraries = vec![
            normal_library("org.example:duplicate:1.0.0"),
            normal_library("org.example:duplicate:1.0.0"),
        ];

        let jobs = library_jobs_for(mc_dir, &libraries, &env);

        assert_eq!(jobs.len(), 1);
        assert!(jobs[0].name.contains("duplicate-1.0.0.jar"));
    }

    #[test]
    fn unique_asset_object_jobs_deduplicate_same_hash() {
        let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
        let hash_a = "abcdef1234567890abcdef1234567890abcdef12";
        let hash_b = "1234567890abcdef1234567890abcdef12345678";

        let jobs = unique_asset_object_jobs(objects_dir, [(hash_a, 4), (hash_a, 4), (hash_b, 8)])
            .expect("valid asset jobs");

        assert_eq!(jobs.len(), 2);
        assert_eq!(jobs[0].hash, hash_a);
        assert_eq!(jobs[0].path, objects_dir.join("ab").join(hash_a));
        assert_eq!(jobs[0].expected, ExpectedIntegrity::from_mojang(4, hash_a));
        assert_eq!(jobs[1].hash, hash_b);
        assert_eq!(jobs[1].path, objects_dir.join("12").join(hash_b));
        assert_eq!(jobs[1].expected, ExpectedIntegrity::from_mojang(8, hash_b));
    }

    #[test]
    fn unique_asset_object_jobs_rejects_one_character_hash() {
        let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
        let result = unique_asset_object_jobs(objects_dir, [("a", 4)]);

        assert!(matches!(result, Err(DownloadError::Integrity(_))));
    }

    #[test]
    fn unique_asset_object_jobs_rejects_non_hex_hash() {
        let objects_dir = Path::new("/tmp/croopor-test/assets/objects");
        let result = unique_asset_object_jobs(
            objects_dir,
            [("abcdef1234567890abcdef1234567890abcdef1z", 4)],
        );

        assert!(matches!(result, Err(DownloadError::Integrity(_))));
    }

    #[tokio::test]
    async fn missing_asset_object_jobs_uses_bounded_size_prefilter() {
        let root = temp_dir("asset-filter");
        let objects_dir = root.join("assets").join("objects");
        let existing_hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let missing_hash = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        let wrong_size_hash = "cccccccccccccccccccccccccccccccccccccccc";
        let existing_path = objects_dir.join("aa").join(existing_hash);
        let missing_path = objects_dir.join("bb").join(missing_hash);
        let wrong_size_path = objects_dir.join("cc").join(wrong_size_hash);
        fs::create_dir_all(existing_path.parent().expect("existing parent"))
            .expect("create existing parent");
        fs::create_dir_all(wrong_size_path.parent().expect("wrong size parent"))
            .expect("create wrong size parent");
        fs::write(&existing_path, b"asset").expect("write existing asset");
        fs::write(&wrong_size_path, b"short").expect("write wrong size asset");

        let jobs = missing_asset_object_jobs(vec![
            AssetObjectDownloadJob {
                hash: existing_hash.to_string(),
                path: existing_path,
                expected: ExpectedIntegrity::from_mojang(5, existing_hash),
            },
            AssetObjectDownloadJob {
                hash: missing_hash.to_string(),
                path: missing_path.clone(),
                expected: ExpectedIntegrity::from_mojang(5, missing_hash),
            },
            AssetObjectDownloadJob {
                hash: wrong_size_hash.to_string(),
                path: wrong_size_path.clone(),
                expected: ExpectedIntegrity::from_mojang(6, wrong_size_hash),
            },
        ])
        .await
        .expect("filter jobs");

        let paths = jobs.into_iter().map(|job| job.path).collect::<HashSet<_>>();

        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&missing_path));
        assert!(paths.contains(&wrong_size_path));

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn existing_file_satisfies_rejects_size_and_sha1_mismatch() {
        let root = temp_dir("existing-integrity");
        fs::create_dir_all(&root).expect("create root");
        let path = root.join("artifact.jar");
        fs::write(&path, b"artifact").expect("write artifact");
        let good_sha1 = sha1_hex(b"artifact");

        assert!(
            existing_file_satisfies(&path, &ExpectedIntegrity::from_mojang(8, &good_sha1))
                .await
                .expect("matching file")
        );
        assert!(
            !existing_file_satisfies(&path, &ExpectedIntegrity::from_mojang(7, &good_sha1))
                .await
                .expect("size mismatch")
        );
        assert!(
            !existing_file_satisfies(
                &path,
                &ExpectedIntegrity::from_mojang(8, "0000000000000000000000000000000000000000")
            )
            .await
            .expect("sha1 mismatch")
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_with_client_rejects_oversized_content_length_before_temp_file() {
        let root = temp_dir("oversized-content-length");
        let destination = root.join("nested").join("artifact.jar");
        let tmp_path = destination.with_extension("tmp");
        let expected =
            ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let url = spawn_download_response_server(
            "200 OK",
            vec![
                (
                    "Content-Type".to_string(),
                    "application/octet-stream".to_string(),
                ),
                ("Content-Length".to_string(), "9".to_string()),
            ],
            b"short".to_vec(),
            3,
        )
        .await;
        let client = build_http_client(Duration::from_secs(5));

        let result = download_file_with_client(&client, &url, &destination, &expected).await;

        assert!(matches!(result, Err(DownloadError::Integrity(_))));
        assert!(!tmp_path.exists());
        assert!(!destination.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_with_client_rejects_stream_past_expected_size_and_cleans_temp() {
        let root = temp_dir("oversized-stream");
        let destination = root.join("nested").join("artifact.jar");
        let tmp_path = destination.with_extension("tmp");
        let expected =
            ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let url = spawn_download_response_server(
            "200 OK",
            vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            b"123456789".to_vec(),
            3,
        )
        .await;
        let client = build_http_client(Duration::from_secs(5));

        let result = download_file_with_client(&client, &url, &destination, &expected).await;

        assert!(matches!(result, Err(DownloadError::Integrity(_))));
        assert!(!tmp_path.exists());
        assert!(!destination.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn download_file_with_client_rejects_streamed_sha1_mismatch_and_cleans_temp() {
        let root = temp_dir("sha1-stream-mismatch");
        let destination = root.join("nested").join("artifact.jar");
        let tmp_path = destination.with_extension("tmp");
        let expected =
            ExpectedIntegrity::from_mojang(8, "0000000000000000000000000000000000000000");
        let url = spawn_download_response_server(
            "200 OK",
            vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            b"artifact".to_vec(),
            3,
        )
        .await;
        let client = build_http_client(Duration::from_secs(5));

        let result = download_file_with_client(&client, &url, &destination, &expected).await;

        assert!(matches!(result, Err(DownloadError::Integrity(_))));
        assert!(!tmp_path.exists());
        assert!(!destination.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn write_download_response_returns_streamed_integrity() {
        let root = temp_dir("write-response-integrity");
        fs::create_dir_all(&root).expect("create root");
        let destination = root.join("artifact.jar");
        let tmp_path = destination.with_extension("tmp");
        let body = b"artifact".to_vec();
        let url = spawn_download_response_server(
            "200 OK",
            vec![(
                "Content-Type".to_string(),
                "application/octet-stream".to_string(),
            )],
            body.clone(),
            1,
        )
        .await;
        let client = build_http_client(Duration::from_secs(5));
        let response = client
            .get(url)
            .send()
            .await
            .expect("download response")
            .error_for_status()
            .expect("successful response");

        let actual = write_download_response(
            response,
            &tmp_path,
            &destination,
            &ExpectedIntegrity::default(),
        )
        .await
        .expect("write response");

        assert_eq!(
            actual,
            ActualIntegrity {
                size: body.len() as u64,
                sha1: Some(sha1_hex(&body)),
            }
        );
        assert_eq!(fs::read(&tmp_path).expect("read temp artifact"), body);
        assert!(!destination.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn download_integrity_futures_stay_small_enough_for_tokio_workers() {
        let path = Path::new("/tmp/croopor-test/artifact.jar");
        let expected =
            ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");

        assert!(
            std::mem::size_of_val(&hash_file(path)) < 4096,
            "hash_file future should not embed the hash buffer on the task stack"
        );
        assert!(
            std::mem::size_of_val(&existing_file_satisfies(path, &expected)) < 4096,
            "existing-file integrity future should stay small"
        );

        let root = temp_dir("install-version-future-size");
        let downloader = Downloader::new(&root);
        assert!(
            std::mem::size_of_val(&downloader.install_version("1.21.1", None, |_| {})) < 8192,
            "version-install future should stay comfortably below tokio worker stack limits"
        );
    }

    #[tokio::test]
    async fn virtual_asset_copy_reports_destination_errors() {
        let root = temp_dir("virtual-asset-copy-error");
        let src = root.join("objects").join("aa").join("asset");
        let dst = root
            .join("virtual")
            .join("legacy")
            .join("sounds")
            .join("step.ogg");
        fs::create_dir_all(src.parent().expect("source parent")).expect("create source parent");
        fs::create_dir_all(&dst).expect("create destination directory");
        fs::write(&src, b"asset").expect("write source asset");

        let result = copy_virtual_asset_if_missing(&src, &dst).await;

        assert!(result.is_err());
        assert!(src.is_file());
        assert!(dst.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn virtual_asset_copy_skips_existing_destination() {
        let root = temp_dir("virtual-asset-copy-existing");
        let src = root.join("objects").join("aa").join("asset");
        let dst = root
            .join("virtual")
            .join("legacy")
            .join("sounds")
            .join("step.ogg");
        fs::create_dir_all(src.parent().expect("source parent")).expect("create source parent");
        fs::create_dir_all(dst.parent().expect("destination parent"))
            .expect("create destination parent");
        fs::write(&src, b"source").expect("write source asset");
        fs::write(&dst, b"existing").expect("write existing virtual asset");

        copy_virtual_asset_if_missing(&src, &dst)
            .await
            .expect("existing virtual asset should be kept");

        assert_eq!(
            fs::read(&dst).expect("read existing virtual asset"),
            b"existing"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn virtual_asset_mapping_copies_multiple_assets() {
        let root = temp_dir("virtual-asset-mapping-copy");
        let objects_dir = root.join("objects");
        let virtual_dir = root.join("virtual").join("legacy");
        let hash_a = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let hash_b = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";
        fs::create_dir_all(objects_dir.join("aa")).expect("create first object parent");
        fs::create_dir_all(objects_dir.join("bb")).expect("create second object parent");
        fs::write(objects_dir.join("aa").join(hash_a), b"step").expect("write first object");
        fs::write(objects_dir.join("bb").join(hash_b), b"hit").expect("write second object");

        copy_virtual_assets(
            &objects_dir,
            &virtual_dir,
            [
                ("sounds/step.ogg".to_string(), hash_a.to_string()),
                ("sounds/hit.ogg".to_string(), hash_b.to_string()),
            ],
        )
        .await
        .expect("copy virtual assets");

        assert_eq!(
            fs::read(virtual_dir.join("sounds").join("step.ogg"))
                .expect("read first virtual asset"),
            b"step"
        );
        assert_eq!(
            fs::read(virtual_dir.join("sounds").join("hit.ogg"))
                .expect("read second virtual asset"),
            b"hit"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn virtual_asset_mapping_rejects_unsafe_provider_paths() {
        let root = temp_dir("virtual-asset-mapping-unsafe");
        let objects_dir = root.join("objects");
        let virtual_dir = root.join("virtual").join("legacy");
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        fs::create_dir_all(objects_dir.join("aa")).expect("create object parent");
        fs::write(objects_dir.join("aa").join(hash), b"asset").expect("write object");

        let result = copy_virtual_assets(
            &objects_dir,
            &virtual_dir,
            [("../escape.ogg".to_string(), hash.to_string())],
        )
        .await;

        assert!(matches!(
            result,
            Err(DownloadError::Integrity(message))
                if message.contains("unsafe virtual asset path")
        ));
        assert!(!root.join("escape.ogg").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn virtual_asset_mapping_reports_destination_errors() {
        let root = temp_dir("virtual-asset-mapping-destination-error");
        let objects_dir = root.join("objects");
        let virtual_dir = root.join("virtual").join("legacy");
        let hash = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
        let dst = virtual_dir.join("sounds").join("step.ogg");
        fs::create_dir_all(objects_dir.join("aa")).expect("create object parent");
        fs::create_dir_all(&dst).expect("create destination directory");
        fs::write(objects_dir.join("aa").join(hash), b"asset").expect("write object");

        let result = copy_virtual_assets(
            &objects_dir,
            &virtual_dir,
            [("sounds/step.ogg".to_string(), hash.to_string())],
        )
        .await;

        assert!(result.is_err());
        assert!(dst.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn virtual_asset_destination_rejects_unsafe_provider_paths() {
        let root = Path::new("/tmp/croopor-test/assets/virtual/legacy");

        assert_eq!(
            virtual_asset_destination(root, "sounds/step.ogg").expect("safe path"),
            root.join("sounds").join("step.ogg")
        );

        for unsafe_name in [
            "",
            "/absolute.ogg",
            "../escape.ogg",
            "sounds/../escape.ogg",
            "sounds//step.ogg",
            "C:\\escape.ogg",
        ] {
            assert!(
                matches!(
                    virtual_asset_destination(root, unsafe_name),
                    Err(DownloadError::Integrity(message))
                        if message.contains("unsafe virtual asset path")
                ),
                "expected unsafe virtual asset path rejection for {unsafe_name:?}"
            );
        }
    }

    #[tokio::test]
    async fn promote_download_temp_replaces_existing_destination() {
        let root = temp_dir("promote-replace");
        fs::create_dir_all(&root).expect("create root");
        let destination = root.join("artifact.jar");
        let temp_path = root.join("artifact.tmp");
        fs::write(&destination, b"stale").expect("write stale artifact");
        fs::write(&temp_path, b"fresh").expect("write temp artifact");

        promote_download_temp(&temp_path, &destination)
            .await
            .expect("promote temp");

        assert_eq!(
            fs::read(&destination).expect("read promoted artifact"),
            b"fresh"
        );
        assert!(!temp_path.exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remove_stale_download_temp_removes_directory() {
        let root = temp_dir("temp-cleanup-dir");
        fs::create_dir_all(root.join("artifact.tmp")).expect("create stale temp directory");

        remove_stale_download_temp(&root.join("artifact.tmp"))
            .await
            .expect("remove stale temp directory");

        assert!(!root.join("artifact.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remove_stale_download_temp_removes_file() {
        let root = temp_dir("temp-cleanup-file");
        fs::create_dir_all(&root).expect("create root");
        fs::write(root.join("artifact.tmp"), b"stale").expect("write stale temp file");

        remove_stale_download_temp(&root.join("artifact.tmp"))
            .await
            .expect("remove stale temp file");

        assert!(!root.join("artifact.tmp").exists());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn remove_stale_download_temp_accepts_missing_path() {
        let root = temp_dir("temp-cleanup-missing");

        remove_stale_download_temp(&root.join("artifact.tmp"))
            .await
            .expect("missing temp path is clean");

        assert!(!root.join("artifact.tmp").exists());
    }

    #[tokio::test]
    async fn promote_download_temp_removes_temp_when_retry_fails() {
        let root = temp_dir("promote-cleanup");
        fs::create_dir_all(&root).expect("create root");
        let destination = root.join("artifact.jar");
        let temp_path = root.join("artifact.tmp");
        fs::create_dir_all(&destination).expect("create destination directory");
        fs::write(&temp_path, b"fresh").expect("write temp artifact");

        let result = promote_download_temp(&temp_path, &destination).await;

        assert!(result.is_err());
        assert!(!temp_path.exists());
        assert!(destination.is_dir());

        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn promote_download_temp_preserves_destination_when_temp_is_missing() {
        let root = temp_dir("promote-missing-temp");
        fs::create_dir_all(&root).expect("create root");
        let destination = root.join("artifact.jar");
        let temp_path = root.join("missing.tmp");
        fs::write(&destination, b"existing").expect("write existing artifact");

        let result = promote_download_temp(&temp_path, &destination).await;

        assert!(result.is_err());
        assert_eq!(
            fs::read(&destination).expect("read existing artifact"),
            b"existing"
        );

        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn verify_download_integrity_rejects_mismatches() {
        let path = Path::new("/tmp/croopor-test/artifact.jar");
        let expected =
            ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let wrong_size = ActualIntegrity {
            size: 7,
            sha1: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        };
        let wrong_sha1 = ActualIntegrity {
            size: 8,
            sha1: Some("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb".to_string()),
        };

        assert!(matches!(
            verify_download_integrity(path, &expected, &wrong_size),
            Err(DownloadIntegrityError::SizeMismatch { .. })
        ));
        assert!(matches!(
            verify_download_integrity(path, &expected, &wrong_sha1),
            Err(DownloadIntegrityError::Sha1Mismatch { .. })
        ));
    }

    #[test]
    fn download_integrity_errors_report_file_name_without_local_path() {
        let path = Path::new("/home/alice/.minecraft/libraries/org/example/lib.jar");
        let expected =
            ExpectedIntegrity::from_mojang(8, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let wrong_size = ActualIntegrity {
            size: 7,
            sha1: Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_string()),
        };

        let message = verify_download_integrity(path, &expected, &wrong_size)
            .expect_err("expected size mismatch")
            .to_string();
        let early_size_message = download_size_mismatch(path, 8, 9).to_string();

        for message in [message, early_size_message] {
            assert!(message.contains("lib.jar"));
            assert!(!message.contains("/home/alice"));
            assert!(!message.contains(".minecraft"));
        }
    }

    #[test]
    fn download_integrity_file_label_falls_back_to_generic_artifact() {
        assert_eq!(bounded_download_file_label(Path::new("/")), "artifact");
    }

    #[test]
    fn library_artifact_job_carries_expected_integrity() {
        let artifact_path = "org/example/lib/1.0.0/lib-1.0.0.jar";
        let sha1 = "abcdef1234567890abcdef1234567890abcdef12";
        let lib = Library {
            name: "org.example:lib:1.0.0".to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(LibraryArtifact {
                    path: artifact_path.to_string(),
                    url: format!("https://libraries.minecraft.net/{artifact_path}"),
                    sha1: sha1.to_string(),
                    size: 1234,
                }),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        };

        let job =
            resolve_library_download(&lib, Path::new("/tmp/croopor-test")).expect("library job");

        assert_eq!(job.expected, ExpectedIntegrity::from_mojang(1234, sha1));
    }

    #[test]
    fn native_classifier_job_carries_expected_integrity() {
        let artifact_path = "org/example/lib/1.0.0/lib-1.0.0-natives-windows.jar";
        let sha1 = "1234567890abcdef1234567890abcdef12345678";
        let mut natives = HashMap::new();
        natives.insert("windows".to_string(), "natives-windows".to_string());
        let mut classifiers = HashMap::new();
        classifiers.insert(
            "natives-windows".to_string(),
            LibraryArtifact {
                path: artifact_path.to_string(),
                url: format!("https://libraries.minecraft.net/{artifact_path}"),
                sha1: sha1.to_string(),
                size: 4321,
            },
        );
        let lib = Library {
            name: "org.example:lib:1.0.0".to_string(),
            downloads: Some(LibraryDownload {
                artifact: None,
                classifiers,
            }),
            natives,
            ..Library::default()
        };

        let job = resolve_native_download(&lib, Path::new("/tmp/croopor-test"), "windows")
            .expect("native job");

        assert_eq!(job.expected, ExpectedIntegrity::from_mojang(4321, sha1));
    }

    #[test]
    fn library_maven_fallback_job_reuses_when_metadata_missing() {
        let lib = Library {
            name: "org.example:lib:1.0.0".to_string(),
            downloads: None,
            ..Library::default()
        };

        let job =
            resolve_library_download(&lib, Path::new("/tmp/croopor-test")).expect("library job");

        assert_eq!(job.expected, ExpectedIntegrity::default());
        assert!(!job.expected.has_evidence());
    }

    #[test]
    fn expected_integrity_ignores_default_mojang_metadata() {
        let expected = ExpectedIntegrity::from_mojang(0, " ");

        assert_eq!(expected, ExpectedIntegrity::default());
        assert!(!expected.has_evidence());
    }

    #[test]
    fn manifest_entry_download_carries_sha1_without_forcing_download() {
        let sha1 = "abcdef1234567890abcdef1234567890abcdef12";
        let download = version_json_download_from_manifest_entry(ManifestEntry {
            id: "1.20.1".to_string(),
            kind: "release".to_string(),
            url: "https://example.invalid/1.20.1.json".to_string(),
            time: String::new(),
            release_time: String::new(),
            sha1: sha1.to_string(),
            compliance_level: 1,
        });

        assert_eq!(download.url, "https://example.invalid/1.20.1.json");
        assert_eq!(download.expected, ExpectedIntegrity::from_sha1(sha1));
        assert!(!download.force_download);
    }

    fn sha1_hex(bytes: &[u8]) -> String {
        let mut hasher = Sha1::new();
        hasher.update(bytes);
        format!("{:x}", hasher.finalize())
    }

    fn native_library(name: &str) -> Library {
        let artifact_path = maven_to_path(name).to_string_lossy().replace('\\', "/");
        Library {
            name: name.to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(artifact(&artifact_path)),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        }
    }

    fn normal_library(name: &str) -> Library {
        let artifact_path = maven_to_path(name).to_string_lossy().replace('\\', "/");
        Library {
            name: name.to_string(),
            downloads: Some(LibraryDownload {
                artifact: Some(artifact(&artifact_path)),
                classifiers: HashMap::new(),
            }),
            ..Library::default()
        }
    }

    fn artifact(path: &str) -> LibraryArtifact {
        LibraryArtifact {
            path: path.to_string(),
            url: format!("https://libraries.minecraft.net/{path}"),
            ..LibraryArtifact::default()
        }
    }

    fn temp_dir(prefix: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|value| value.as_nanos())
            .unwrap_or_default();
        std::env::temp_dir().join(format!(
            "croopor-download-{prefix}-{}-{nanos:x}",
            std::process::id()
        ))
    }

    async fn spawn_download_response_server(
        status: &str,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
        responses: usize,
    ) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind download response server");
        let url = format!("http://{}", listener.local_addr().expect("local addr"));
        let status = status.to_string();
        tokio::spawn(async move {
            for _ in 0..responses {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let mut response = format!("HTTP/1.1 {status}\r\nConnection: close\r\n");
                for (name, value) in &headers {
                    response.push_str(&format!("{name}: {value}\r\n"));
                }
                response.push_str("\r\n");
                let _ = socket.write_all(response.as_bytes()).await;
                let _ = socket.write_all(&body).await;
            }
        });
        url
    }

    async fn spawn_overlapped_install_server()
    -> (String, mpsc::UnboundedReceiver<String>, oneshot::Sender<()>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind install overlap server");
        let base_url = format!("http://{}", listener.local_addr().expect("local addr"));
        let (request_tx, request_rx) = mpsc::unbounded_channel();
        let (release_library_tx, release_library_rx) = oneshot::channel();
        let library_body = b"library".to_vec();
        let library_sha1 = sha1_hex(&library_body);
        let asset_index_body = br#"{"objects":{}}"#.to_vec();
        let asset_index_sha1 = sha1_hex(&asset_index_body);
        let version_body = serde_json::json!({
            "id": "overlap",
            "assetIndex": {
                "id": "overlap-assets",
                "sha1": asset_index_sha1,
                "size": asset_index_body.len(),
                "url": format!("{base_url}/asset-index.json")
            },
            "libraries": [{
                "name": "org.example:lib:1.0.0",
                "downloads": {
                    "artifact": {
                        "path": "org/example/lib/1.0.0/lib-1.0.0.jar",
                        "url": format!("{base_url}/libraries/lib.jar"),
                        "sha1": library_sha1,
                        "size": library_body.len()
                    }
                }
            }]
        })
        .to_string()
        .into_bytes();

        tokio::spawn(async move {
            let mut release_library_rx = Some(release_library_rx);
            for _ in 0..4 {
                let Ok((mut socket, _)) = listener.accept().await else {
                    return;
                };
                let request_path = match read_request_path(&mut socket).await {
                    Some(path) => path,
                    None => return,
                };
                let _ = request_tx.send(request_path.clone());
                let body = match request_path.as_str() {
                    "/version.json" => version_body.clone(),
                    "/asset-index.json" => asset_index_body.clone(),
                    "/libraries/lib.jar" => {
                        if let Some(receiver) = release_library_rx.take() {
                            let _ = receiver.await;
                        }
                        library_body.clone()
                    }
                    _ => {
                        write_raw_response(&mut socket, "404 Not Found", b"not found").await;
                        continue;
                    }
                };
                write_raw_response(&mut socket, "200 OK", &body).await;
            }
        });

        (
            format!("{base_url}/version.json"),
            request_rx,
            release_library_tx,
        )
    }

    async fn read_request_path(socket: &mut tokio::net::TcpStream) -> Option<String> {
        let mut buffer = vec![0_u8; 1024];
        let mut received = Vec::new();
        loop {
            let read = socket.read(&mut buffer).await.ok()?;
            if read == 0 {
                return None;
            }
            received.extend_from_slice(&buffer[..read]);
            if received.windows(4).any(|window| window == b"\r\n\r\n") {
                break;
            }
        }
        let request = String::from_utf8_lossy(&received);
        request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .map(ToOwned::to_owned)
    }

    async fn write_raw_response(socket: &mut tokio::net::TcpStream, status: &str, body: &[u8]) {
        let response = format!(
            "HTTP/1.1 {status}\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.write_all(body).await;
    }
}
