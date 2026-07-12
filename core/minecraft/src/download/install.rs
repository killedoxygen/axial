use super::assets::{
    abort_asset_download_pipeline, await_asset_download_pipeline, recv_asset_progress,
    spawn_asset_download_pipeline,
};
use super::client::{library_download_concurrency, standard_minecraft_download_client};
use super::libraries::{DownloadJob, decode_sha1, library_jobs_for};
use super::model::{
    DownloadError, DownloadProgress, ExactLibraryDownloadProof, ExecutionDownloadFact,
    ExpectedIntegrity, LibraryPlanError, SelectedDownloadArtifactDescriptor,
    SelectedDownloadArtifactKind, progress,
};
use super::plan::TransferPlan;
use super::runtime::{
    finish_runtime_pipeline_after_artifacts, recv_runtime_progress, spawn_runtime_ensure_pipeline,
};
use super::transfer::{
    VerifiedSelectedArtifactDownload, ensure_selected_artifact_with_client_and_observed_size,
    fetch_verified_selected_artifact_bytes_with_client,
};
use crate::artifact_path::validate_artifact_path_segment;
use crate::known_good::{
    KnownGoodInstallReceipt, KnownGoodInstallShape, KnownGoodInventoryInput,
    MAX_KNOWN_GOOD_ASSET_INDEX_BYTES, MAX_KNOWN_GOOD_VERSION_JSON_BYTES, RuntimeInventoryInput,
    seal_verified_vanilla_library_authority,
};
use crate::launch::{VersionJson, effective_java_version_for};
use crate::manifest::{ManifestEntry, VersionManifest, fetch_fresh_install_version_manifest};
use crate::paths::{assets_dir, versions_dir};
use crate::rules::default_environment;
use futures_util::StreamExt;
use std::io;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::fs as async_fs;
use tokio::sync::mpsc;

pub struct Downloader {
    mc_dir: PathBuf,
    client: reqwest::Client,
    #[cfg(test)]
    install_manifest: Option<VersionManifest>,
}

struct VerifiedVanillaArtifacts {
    client_size: u64,
    library_proofs: Vec<ExactLibraryDownloadProof>,
    log_config_size: Option<u64>,
}

impl Downloader {
    pub fn new(mc_dir: impl Into<PathBuf>) -> Self {
        Self {
            mc_dir: mc_dir.into(),
            client: standard_minecraft_download_client(),
            #[cfg(test)]
            install_manifest: None,
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
        self.install_version_with_fact_sender(version_id, &mut send, None, None)
            .await
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
        let result = self
            .install_version_with_fact_sender(
                version_id,
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
            async_fs::create_dir_all(&version_dir).await?;
            async_fs::write(&marker_path, b"installing").await?;
            self.install_version_inner(
                version_id,
                version_manifest_entry,
                &mut send,
                &plan,
                fact_tx.as_ref(),
                descriptor_tx.as_ref(),
            )
            .await
        }
        .await;

        match install_result {
            Ok(receipt) => {
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
        version_manifest_entry: ManifestEntry,
        send: &mut F,
        plan: &Arc<TransferPlan>,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<KnownGoodInstallReceipt, DownloadError>
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

        let version_json_expected = ExpectedIntegrity::from_sha1(&version_manifest_entry.sha1);
        let version_json_bytes =
            fetch_verified_selected_artifact_bytes_with_client(VerifiedSelectedArtifactDownload {
                kind: SelectedDownloadArtifactKind::VersionJson,
                client: &self.client,
                url: &version_manifest_entry.url,
                destination: &json_path,
                expected: &version_json_expected,
                max_bytes: MAX_KNOWN_GOOD_VERSION_JSON_BYTES,
                fact_tx,
                descriptor_tx,
            })
            .await?;

        let version = parse_vanilla_version_source(&version_json_bytes, version_id)?;
        let asset_index_bytes = self
            .fetch_asset_index_source(&version, send, plan, fact_tx, descriptor_tx)
            .await?;
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
                    let (_, observed_size) = ensure_selected_artifact_with_client_and_observed_size(
                        SelectedDownloadArtifactKind::ClientJar,
                        &http_client,
                        &url,
                        &jar_path,
                        &expected,
                        fact_tx.as_ref(),
                        descriptor_tx.as_ref(),
                    )
                    .await?;
                    Ok::<u64, DownloadError>(observed_size)
                }))
            } else {
                None
            };
            let mut asset_pipeline = asset_index_bytes.clone().map(|verified_index_bytes| {
                spawn_asset_download_pipeline(
                    self.mc_dir.clone(),
                    self.client.clone(),
                    verified_index_bytes,
                    fact_tx.cloned(),
                    descriptor_tx.cloned(),
                    plan.clone(),
                )
            });

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
                let mut proofs = Vec::with_capacity(total_library_jobs as usize);
                let mut library_downloads =
                    futures_util::stream::iter(library_jobs.into_iter().map(|job| {
                        let client = client.clone();
                        let fact_tx = fact_tx.cloned();
                        let descriptor_tx = descriptor_tx.cloned();
                        async move {
                            let (_, observed_size) = ensure_selected_artifact_with_client_and_observed_size(
                                SelectedDownloadArtifactKind::Library,
                                &client,
                                &job.url,
                                &job.path,
                                &job.expected,
                                fact_tx.as_ref(),
                                descriptor_tx.as_ref(),
                            )
                            .await?;
                            let sha1 = job
                                .expected
                                .sha1
                                .as_deref()
                                .and_then(decode_sha1)
                                .ok_or(LibraryPlanError::InvalidChecksum)?;
                            let proof = ExactLibraryDownloadProof::new(
                                job.relative_path,
                                observed_size,
                                sha1,
                            );
                            Ok::<_, DownloadError>((job.name, observed_size, proof))
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
                            proofs.push(proof);
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
            let client_size = client_jar_result?.ok_or_else(|| {
                DownloadError::ResolveManifest(
                    "installed version has no authenticated client artifact".to_string(),
                )
            })?;
            let library_proofs = library_result?;

            let log_config_size = if let Some(logging) = version
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
                let (_, observed_size) = ensure_selected_artifact_with_client_and_observed_size(
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
                Some(observed_size)
            } else {
                None
            };
            Ok::<_, DownloadError>(VerifiedVanillaArtifacts {
                client_size,
                library_proofs,
                log_config_size,
            })
        }
        .await;

        let (runtime_receipt, verified_artifacts) =
            finish_runtime_pipeline_after_artifacts(runtime_pipeline, artifact_result, send)
                .await?;

        let runtime_expected = runtime_receipt.as_ref().map(|receipt| ExpectedIntegrity {
            size: Some(receipt.expected_size()),
            sha1: Some(receipt.expected_sha1().to_string()),
        });
        let environment = default_environment();
        let library_authority = seal_verified_vanilla_library_authority(
            &version.libraries,
            &environment,
            verified_artifacts.library_proofs,
        )
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "installed library authority could not be sealed: {error:?}"
            ))
        })?;
        KnownGoodInstallReceipt::from_verified_vanilla_source(
            KnownGoodInventoryInput {
                resolved_version: &version,
                version_metadata_size: version_json_bytes.len() as u64,
                client_size: verified_artifacts.client_size,
                libraries: &library_authority,
                log_config_size: verified_artifacts.log_config_size,
                asset_index_bytes: asset_index_bytes.as_deref(),
                runtime: runtime_receipt.as_ref().zip(runtime_expected.as_ref()).map(
                    |(receipt, expected)| RuntimeInventoryInput {
                        component: receipt.component(),
                        manifest_bytes: receipt.bytes(),
                        manifest_expected: expected,
                    },
                ),
                shape: KnownGoodInstallShape {
                    version_manifest: &version_manifest_entry,
                },
                environment: &environment,
            },
            &version_json_bytes,
        )
        .map_err(|error| {
            DownloadError::ResolveManifest(format!(
                "installed source inventory could not be derived: {error:?}"
            ))
        })
    }

    async fn fetch_asset_index_source<F>(
        &self,
        version: &VersionJson,
        send: &mut F,
        plan: &TransferPlan,
        fact_tx: Option<&mpsc::UnboundedSender<ExecutionDownloadFact>>,
        descriptor_tx: Option<&mpsc::UnboundedSender<SelectedDownloadArtifactDescriptor>>,
    ) -> Result<Option<Arc<[u8]>>, DownloadError>
    where
        F: FnMut(DownloadProgress),
    {
        if version.asset_index.url.trim().is_empty() {
            return if version.asset_index.id.trim().is_empty() {
                Ok(None)
            } else {
                Err(DownloadError::ResolveManifest(
                    "version asset index has no download source".to_string(),
                ))
            };
        }
        let index_name = format!("{}.json", version.asset_index.id);
        if validate_artifact_path_segment(&version.asset_index.id).is_err()
            || validate_artifact_path_segment(&index_name).is_err()
        {
            return Err(DownloadError::ResolveManifest(
                "version asset index has an invalid identity".to_string(),
            ));
        }
        let index_path = assets_dir(&self.mc_dir).join("indexes").join(&index_name);
        let expected =
            ExpectedIntegrity::from_mojang(version.asset_index.size, &version.asset_index.sha1);
        let planned_bytes = expected.size.unwrap_or(0);
        plan.contribute_total(planned_bytes);
        send(progress("asset_index", 0, 1, Some(index_name.clone())));
        let bytes =
            fetch_verified_selected_artifact_bytes_with_client(VerifiedSelectedArtifactDownload {
                kind: SelectedDownloadArtifactKind::AssetIndex,
                client: &self.client,
                url: &version.asset_index.url,
                destination: &index_path,
                expected: &expected,
                max_bytes: MAX_KNOWN_GOOD_ASSET_INDEX_BYTES,
                fact_tx,
                descriptor_tx,
            })
            .await?;
        plan.add_done(planned_bytes);
        send(progress("asset_index", 1, 1, Some(index_name)));
        Ok(Some(bytes))
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

    fn library_jobs(&self, version: &VersionJson) -> Result<Vec<DownloadJob>, DownloadError> {
        let env = default_environment();
        Ok(library_jobs_for(&self.mc_dir, &version.libraries, &env)?)
    }
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
    task: Option<tokio::task::JoinHandle<Result<u64, DownloadError>>>,
) -> Result<Option<u64>, DownloadError> {
    let Some(task) = task else {
        return Ok(None);
    };

    task.await.map_err(client_jar_task_error)?.map(Some)
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
