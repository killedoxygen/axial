use super::assets::{
    abort_asset_download_pipeline, await_asset_download_pipeline, recv_asset_progress,
    spawn_asset_download_pipeline,
};
use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::libraries::{DownloadJob, LibraryChecksumPolicy, library_jobs_for};
use super::model::{
    DownloadError, DownloadProgress, ExecutionDownloadFact, ExecutionDownloadReport,
    ExpectedIntegrity, SelectedDownloadArtifactDescriptor, SelectedDownloadArtifactKind, progress,
};
use super::path_safety::path_is_file;
use super::plan::TransferPlan;
use super::runtime::{
    finish_runtime_pipeline_after_artifacts, recv_runtime_progress, spawn_runtime_ensure_pipeline,
};
use super::transfer::{
    download_file_with_client_and_fact_sender,
    download_file_with_client_and_fact_sender_allowing_missing_checksum,
    ensure_selected_artifact_with_client,
};
use crate::launch::{VersionJson, resolve_version};
use crate::manifest::{ManifestEntry, fetch_version_manifest_cached};
use crate::paths::{assets_dir, versions_dir};
use crate::rules::default_environment;
use futures_util::StreamExt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs as async_fs;
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub(super) struct VersionJsonDownload {
    pub(super) url: String,
    pub(super) expected: ExpectedIntegrity,
    pub(super) force_download: bool,
}

pub struct Downloader {
    mc_dir: PathBuf,
    client: reqwest::Client,
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
        self.install_version_with_fact_sender(version_id, manifest_url, &mut send, None, None)
            .await
    }

    pub async fn install_version_with_facts<F, G>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        send: F,
        send_fact: G,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
        G: FnMut(ExecutionDownloadFact),
    {
        self.install_version_with_facts_and_descriptors(
            version_id,
            manifest_url,
            send,
            send_fact,
            |_| {},
        )
        .await
    }

    pub async fn install_version_with_facts_and_descriptors<F, G, H>(
        &self,
        version_id: &str,
        manifest_url: Option<&str>,
        mut send: F,
        mut send_fact: G,
        mut send_descriptor: H,
    ) -> Result<(), DownloadError>
    where
        F: FnMut(DownloadProgress),
        G: FnMut(ExecutionDownloadFact),
        H: FnMut(SelectedDownloadArtifactDescriptor),
    {
        let (fact_tx, mut fact_rx) = mpsc::unbounded_channel();
        let (descriptor_tx, mut descriptor_rx) = mpsc::unbounded_channel();
        let result = self
            .install_version_with_fact_sender(
                version_id,
                manifest_url,
                &mut send,
                Some(fact_tx),
                Some(descriptor_tx),
            )
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
        manifest_url: Option<&str>,
        send: &mut F,
        fact_tx: Option<mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<(), DownloadError>
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
            async_fs::create_dir_all(&version_dir).await?;
            async_fs::write(&marker_path, b"installing").await?;
            self.install_version_inner(
                version_id,
                manifest_url,
                &mut send,
                &plan,
                fact_tx.as_ref(),
                descriptor_tx.as_ref(),
            )
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
                    bytes_done: None,
                    bytes_total: None,
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
        manifest_url: Option<&str>,
        send: &mut F,
        plan: &Arc<TransferPlan>,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
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
                    Err(_) if path_is_file(&json_path).await => VersionJsonDownload {
                        url: String::new(),
                        expected: ExpectedIntegrity::default(),
                        force_download: false,
                    },
                    Err(error) => return Err(error),
                }
            };
        let should_download_version_json =
            !version_json_download.url.is_empty() && version_json_download.force_download;
        if should_download_version_json {
            if version_json_download.expected.sha1.is_none() {
                download_file_with_client_and_fact_sender_allowing_missing_checksum(
                    SelectedDownloadArtifactKind::VersionJson,
                    &self.client,
                    &version_json_download.url,
                    &json_path,
                    &version_json_download.expected,
                    fact_tx,
                    descriptor_tx,
                )
                .await?;
            } else {
                self.download_file(
                    SelectedDownloadArtifactKind::VersionJson,
                    &version_json_download.url,
                    &json_path,
                    &version_json_download.expected,
                    fact_tx,
                    descriptor_tx,
                )
                .await?;
            }
        } else if !version_json_download.url.is_empty() {
            self.ensure_artifact(
                SelectedDownloadArtifactKind::VersionJson,
                &version_json_download.url,
                &json_path,
                &version_json_download.expected,
                fact_tx,
                descriptor_tx,
            )
            .await?;
        }

        let version = resolve_version(&self.mc_dir, version_id)
            .map_err(|error| DownloadError::ResolveManifest(error.to_string()))?;
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
            .filter(|client| !client.file.url.is_empty())
            .and_then(|client| {
                ExpectedIntegrity::from_mojang(client.file.size, &client.file.sha1).size
            })
            .unwrap_or(0);
        plan.contribute_total(log_config_bytes);
        let mut runtime_pipeline = if version.java_version.major_version > 0 {
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

            let mc_dir = self.mc_dir.clone();
            let java_version = version.java_version.clone();
            Some(spawn_runtime_ensure_pipeline(
                mc_dir,
                java_version,
                plan.clone(),
            ))
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
                let fact_tx = fact_tx.cloned();
                let descriptor_tx = descriptor_tx.cloned();
                Some(tokio::spawn(async move {
                    ensure_selected_artifact_with_client(
                        SelectedDownloadArtifactKind::ClientJar,
                        &http_client,
                        &url,
                        &jar_path,
                        &expected,
                        fact_tx.as_ref(),
                        descriptor_tx.as_ref(),
                    )
                    .await?;
                    Ok::<(), DownloadError>(())
                }))
            } else {
                None
            };
            let mut asset_pipeline = spawn_asset_download_pipeline(
                self.mc_dir.clone(),
                self.client.clone(),
                version.asset_index.clone(),
                fact_tx.cloned(),
                descriptor_tx.cloned(),
                plan.clone(),
            );

            let library_jobs = self.library_jobs(&version)?;
            plan.contribute_total(
                library_jobs
                    .iter()
                    .map(|job| job.expected.size.unwrap_or(0))
                    .sum::<u64>(),
            );
            send(progress("libraries", 0, library_jobs.len() as i32, None));
            let client = self.client.clone();
            let total_library_jobs = library_jobs.len() as i32;
            let mut completed_library_jobs = 0;
            let library_result = async {
                let mut library_downloads =
                    futures_util::stream::iter(library_jobs.into_iter().map(|job| {
                        let client = client.clone();
                        let fact_tx = fact_tx.cloned();
                        let descriptor_tx = descriptor_tx.cloned();
                        async move {
                            let bytes = job.expected.size.unwrap_or(0);
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
                            Ok::<(String, u64), DownloadError>((job.name, bytes))
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
                            let (name, bytes) = result?;
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
                Ok::<(), DownloadError>(())
            }
            .await;
            let client_jar_result = await_client_jar_download(client_jar_task).await;
            if client_jar_result.is_ok() && version.downloads.client.is_some() {
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
                self.ensure_artifact(
                    SelectedDownloadArtifactKind::LogConfig,
                    &logging.file.url,
                    &log_config_path,
                    &expected,
                    fact_tx,
                    descriptor_tx,
                )
                .await?;
                plan.add_done(log_config_bytes);
            }
            Ok::<(), DownloadError>(())
        }
        .await;

        let _ = finish_runtime_pipeline_after_artifacts(runtime_pipeline, artifact_result, send)
            .await?;

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

    fn library_jobs(&self, version: &VersionJson) -> Result<Vec<DownloadJob>, DownloadError> {
        let env = default_environment();
        Ok(library_jobs_for(
            &self.mc_dir,
            &version.libraries,
            &env,
            LibraryChecksumPolicy::Strict,
        )?)
    }

    async fn download_file(
        &self,
        kind: SelectedDownloadArtifactKind,
        url: &str,
        destination: &Path,
        expected: &ExpectedIntegrity,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<ExecutionDownloadReport, DownloadError> {
        download_file_with_client_and_fact_sender(
            kind,
            &self.client,
            url,
            destination,
            expected,
            fact_tx,
            descriptor_tx,
        )
        .await
    }

    async fn ensure_artifact(
        &self,
        kind: SelectedDownloadArtifactKind,
        url: &str,
        destination: &Path,
        expected: &ExpectedIntegrity,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<Option<ExecutionDownloadReport>, DownloadError> {
        ensure_selected_artifact_with_client(
            kind,
            &self.client,
            url,
            destination,
            expected,
            fact_tx,
            descriptor_tx,
        )
        .await
    }
}

pub(super) fn version_json_download_from_manifest_entry(
    entry: ManifestEntry,
) -> VersionJsonDownload {
    VersionJsonDownload {
        url: entry.url,
        expected: ExpectedIntegrity::from_sha1(&entry.sha1),
        force_download: false,
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
