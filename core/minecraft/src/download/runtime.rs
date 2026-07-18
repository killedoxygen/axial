use super::model::{DownloadError, DownloadProgress, progress};
use super::plan::{TransferPlan, TransferPlanContribution};
use crate::launch::JavaVersion;
use crate::runtime::{
    JavaRuntimeLookupError, ManagedRuntimeCache, RuntimeEnsureEvent,
    RuntimeMaterializationCancelHandle, RuntimeSourceReceipt, materialize_preferred_runtime_source,
    runtime_materialization_control,
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) struct RuntimeEnsurePipeline {
    task: Option<tokio::task::JoinHandle<RuntimeEnsureTaskOutcome>>,
    cancellation: RuntimeMaterializationCancelHandle,
    progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
    progress_open: bool,
}

enum RuntimeEnsureTaskOutcome {
    Complete(Result<RuntimeSourceReceipt, JavaRuntimeLookupError>),
    Cancelled,
}

impl RuntimeEnsurePipeline {
    pub(super) fn is_finished(&self) -> bool {
        self.task
            .as_ref()
            .is_some_and(tokio::task::JoinHandle::is_finished)
    }

    async fn cancel_or_settle(mut self) {
        let _ = self.cancellation.cancel_before_publication();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for RuntimeEnsurePipeline {
    fn drop(&mut self) {
        let _ = self.cancellation.cancel_before_publication();
        let Some(task) = self.task.take() else {
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let _ = task.await;
            });
        }
        // Outside a Tokio runtime, dropping the join handle still leaves the
        // task detached under the runtime that owns it.
    }
}

pub(super) enum RuntimeEnsurePipelineEvent {
    Progress(DownloadProgress),
    Complete {
        result: Result<RuntimeSourceReceipt, DownloadError>,
        final_progress: Vec<DownloadProgress>,
    },
}

pub(super) fn spawn_runtime_ensure_pipeline(
    runtime_cache: ManagedRuntimeCache,
    java_version: JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    plan: Arc<TransferPlan>,
    contribution: TransferPlanContribution,
) -> RuntimeEnsurePipeline {
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let (cancellation, mut task_control) = runtime_materialization_control();
    let task = tokio::spawn(async move {
        let event_java_version = java_version.clone();
        let progress_tx = progress_tx.clone();
        let mut contribution = Some(contribution);
        let mut plan_done_seen = 0_u64;
        let source_receipt = materialize_preferred_runtime_source(
            &runtime_cache,
            &java_version,
            source_receipt,
            &mut |event| {
                match &event {
                    RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
                        bytes_done,
                        bytes_total,
                        ..
                    } => {
                        if *bytes_total > 0
                            && let Some(contribution) = contribution.take()
                        {
                            contribution.resolve(*bytes_total);
                        }
                        if *bytes_done > plan_done_seen {
                            plan.add_done(*bytes_done - plan_done_seen);
                            plan_done_seen = *bytes_done;
                        }
                    }
                    RuntimeEnsureEvent::ManagedRuntimeReady { .. } => {
                        if let Some(contribution) = contribution.take() {
                            contribution.resolve(0);
                        }
                    }
                    RuntimeEnsureEvent::DownloadingManagedRuntime { .. } => {}
                }
                let _ = progress_tx.send(runtime_ensure_progress(&event_java_version, event));
            },
            &mut task_control,
        )
        .await;
        let result = match source_receipt {
            Ok(Some(source_receipt)) => {
                if let Some(contribution) = contribution.take() {
                    contribution.resolve(0);
                }
                Ok(source_receipt)
            }
            Ok(None) => return RuntimeEnsureTaskOutcome::Cancelled,
            Err(error) => Err(error),
        };
        if task_control.finish() {
            RuntimeEnsureTaskOutcome::Complete(result)
        } else {
            RuntimeEnsureTaskOutcome::Cancelled
        }
    });

    RuntimeEnsurePipeline {
        task: Some(task),
        cancellation,
        progress_rx,
        progress_open: true,
    }
}

#[cfg(test)]
pub(super) fn spawn_test_runtime_source_pipeline(
    source_receipt: RuntimeSourceReceipt,
    contribution: TransferPlanContribution,
) -> RuntimeEnsurePipeline {
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let (cancellation, task_control) = runtime_materialization_control();
    let task = tokio::spawn(async move {
        contribution.resolve(0);
        drop(progress_tx);
        if task_control.finish() {
            RuntimeEnsureTaskOutcome::Complete(Ok(source_receipt))
        } else {
            RuntimeEnsureTaskOutcome::Cancelled
        }
    });

    RuntimeEnsurePipeline {
        task: Some(task),
        cancellation,
        progress_rx,
        progress_open: true,
    }
}

pub(super) fn runtime_ensure_progress(
    java_version: &JavaVersion,
    event: RuntimeEnsureEvent,
) -> DownloadProgress {
    match event {
        RuntimeEnsureEvent::DownloadingManagedRuntime { component } => progress(
            "java_runtime",
            0,
            0,
            Some(format!(
                "Downloading {} (Java {})",
                runtime_component_label(&component),
                java_version.major_version
            )),
        ),
        RuntimeEnsureEvent::InstallingManagedRuntimeFiles { current, total, .. } => progress(
            "java_runtime",
            bounded_progress_count(current),
            bounded_progress_count(total),
            None,
        ),
        RuntimeEnsureEvent::ManagedRuntimeReady { .. } => {
            progress("java_runtime_ready", 1, 1, None)
        }
    }
}

fn runtime_component_label(component: &str) -> String {
    if component.trim().is_empty() {
        "managed runtime".to_string()
    } else {
        component.to_string()
    }
}

pub(super) fn bounded_progress_count(value: usize) -> i32 {
    i32::try_from(value).unwrap_or(i32::MAX)
}

pub(super) async fn next_runtime_pipeline_event(
    pipeline: &mut RuntimeEnsurePipeline,
) -> RuntimeEnsurePipelineEvent {
    loop {
        enum PipelineEvent {
            Progress(Option<DownloadProgress>),
            Complete(Result<RuntimeEnsureTaskOutcome, tokio::task::JoinError>),
        }
        let event = if pipeline.progress_open {
            let task = pipeline
                .task
                .as_mut()
                .expect("live runtime pipeline owns its task");
            tokio::select! {
                biased;
                result = task => PipelineEvent::Complete(result),
                progress = pipeline.progress_rx.recv() => PipelineEvent::Progress(progress),
            }
        } else {
            let result = pipeline
                .task
                .as_mut()
                .expect("live runtime pipeline owns its task")
                .await;
            PipelineEvent::Complete(result)
        };
        match event {
            PipelineEvent::Progress(Some(progress)) => {
                return RuntimeEnsurePipelineEvent::Progress(progress);
            }
            PipelineEvent::Progress(None) => {
                pipeline.progress_open = false;
            }
            PipelineEvent::Complete(result) => {
                pipeline.task.take();
                let result = match result {
                    Ok(RuntimeEnsureTaskOutcome::Complete(result)) => {
                        result.map_err(runtime_lookup_error_to_download_error)
                    }
                    Ok(RuntimeEnsureTaskOutcome::Cancelled) => Err(DownloadError::PrepareRuntime(
                        "runtime preparation was cancelled without a sibling failure".to_string(),
                    )),
                    Err(error) => Err(DownloadError::PrepareRuntime(error.to_string())),
                };
                let final_progress = if result.is_ok() {
                    std::iter::from_fn(|| pipeline.progress_rx.try_recv().ok()).collect()
                } else {
                    Vec::new()
                };
                return RuntimeEnsurePipelineEvent::Complete {
                    result,
                    final_progress,
                };
            }
        }
    }
}

#[cfg(test)]
pub(super) async fn settle_runtime_pipeline(
    mut pipeline: Option<RuntimeEnsurePipeline>,
) -> Result<Option<RuntimeSourceReceipt>, DownloadError> {
    let Some(pipeline) = pipeline.as_mut() else {
        return Ok(None);
    };
    loop {
        match next_runtime_pipeline_event(pipeline).await {
            RuntimeEnsurePipelineEvent::Progress(_) => {}
            RuntimeEnsurePipelineEvent::Complete { result, .. } => return result.map(Some),
        }
    }
}

pub(super) async fn settle_runtime_pipeline_after_failure(
    pipeline: Option<RuntimeEnsurePipeline>,
    primary_error: DownloadError,
) -> DownloadError {
    if let Some(pipeline) = pipeline {
        pipeline.cancel_or_settle().await;
    }
    primary_error
}

#[cfg(test)]
pub(super) fn runtime_pipeline_for_test(
    task: tokio::task::JoinHandle<
        Result<RuntimeSourceReceipt, crate::runtime::JavaRuntimeLookupError>,
    >,
    settlement_owned: bool,
) -> RuntimeEnsurePipeline {
    let (cancellation, task_control) = runtime_materialization_control();
    if settlement_owned {
        assert!(task_control.claim_publication_settlement());
    }
    let task = tokio::spawn(async move {
        let result = match task.await {
            Ok(result) => result,
            Err(error) => Err(JavaRuntimeLookupError::Install(error.to_string())),
        };
        if task_control.finish() {
            RuntimeEnsureTaskOutcome::Complete(result)
        } else {
            RuntimeEnsureTaskOutcome::Cancelled
        }
    });
    let (_progress_tx, progress_rx) = mpsc::unbounded_channel();
    RuntimeEnsurePipeline {
        task: Some(task),
        cancellation,
        progress_rx,
        progress_open: true,
    }
}

pub(super) fn runtime_lookup_error_to_download_error(
    error: JavaRuntimeLookupError,
) -> DownloadError {
    match error {
        JavaRuntimeLookupError::UnsupportedPlatform {
            component,
            platform,
        } => DownloadError::RuntimeUnavailableForPlatform {
            component,
            platform,
        },
        JavaRuntimeLookupError::RosettaRequired { component } => {
            DownloadError::RuntimeRosettaRequired { component }
        }
        JavaRuntimeLookupError::RuntimeSource(failure) => DownloadError::RuntimeSource(failure),
        error => DownloadError::PrepareRuntime(error.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::runtime::{
        ComponentManifest, ComponentManifestDownload, ComponentManifestDownloads,
        ComponentManifestFile, RuntimeId, RuntimeMaterializationCancellation,
        authenticated_runtime_source_from_manifest_for_test,
        block_runtime_before_publication_claim_for_test, block_runtime_publication_for_test,
        component_manifest_proof_bytes, runtime_java_relative_path,
        runtime_publication_locks_available_for_test,
    };
    use sha1::{Digest as _, Sha1};
    use std::collections::HashMap;
    use std::fs;
    use std::path::Path;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio::net::TcpListener;
    use tokio::sync::oneshot;
    use tokio::time::{Duration, timeout};

    struct ExpectedRuntimeTree {
        java_bytes: Vec<u8>,
        config_bytes: Vec<u8>,
        proof_bytes: Vec<u8>,
    }

    fn controlled_pipeline(
        cancellation: RuntimeMaterializationCancelHandle,
        task: tokio::task::JoinHandle<RuntimeEnsureTaskOutcome>,
    ) -> RuntimeEnsurePipeline {
        let (_progress_tx, progress_rx) = mpsc::unbounded_channel();
        RuntimeEnsurePipeline {
            task: Some(task),
            cancellation,
            progress_rx,
            progress_open: true,
        }
    }

    async fn serve_runtime_file(body: Vec<u8>) -> String {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("runtime test listener");
        let address = listener
            .local_addr()
            .expect("runtime test listener address");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("runtime test connection");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let headers = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(headers.as_bytes())
                .await
                .expect("runtime test response headers");
            socket
                .write_all(&body)
                .await
                .expect("runtime test response body");
        });
        format!("http://{address}/runtime.bin")
    }

    fn runtime_manifest_file(url: String, body: &[u8], executable: bool) -> ComponentManifestFile {
        ComponentManifestFile {
            kind: "file".to_string(),
            executable,
            downloads: Some(ComponentManifestDownloads {
                raw: Some(ComponentManifestDownload {
                    url,
                    sha1: Some(format!("{:x}", Sha1::digest(body))),
                    size: Some(body.len() as u64),
                }),
                lzma: None,
            }),
            target: None,
        }
    }

    async fn runtime_pipeline_fixture(
        cache: ManagedRuntimeCache,
    ) -> (RuntimeEnsurePipeline, ExpectedRuntimeTree) {
        let java_bytes = b"published java runtime".to_vec();
        let config_bytes = b"-server KNOWN".to_vec();
        let java_url = serve_runtime_file(java_bytes.clone()).await;
        let config_url = serve_runtime_file(config_bytes.clone()).await;
        let manifest = ComponentManifest {
            files: HashMap::from([
                (
                    runtime_java_relative_path().to_string(),
                    runtime_manifest_file(java_url, &java_bytes, true),
                ),
                (
                    "lib/jvm.cfg".to_string(),
                    runtime_manifest_file(config_url, &config_bytes, false),
                ),
            ]),
        };
        let proof_bytes = component_manifest_proof_bytes(&manifest).expect("runtime proof fixture");
        let source = authenticated_runtime_source_from_manifest_for_test(
            RuntimeId::from("java-runtime-delta"),
            manifest,
        )
        .expect("authenticated runtime source fixture");
        let plan = TransferPlan::shared();
        let contribution = plan.reserve_contribution();
        let pipeline = spawn_runtime_ensure_pipeline(
            cache,
            JavaVersion {
                component: "java-runtime-delta".to_string(),
                major_version: 21,
            },
            source,
            plan,
            contribution,
        );
        (
            pipeline,
            ExpectedRuntimeTree {
                java_bytes,
                config_bytes,
                proof_bytes,
            },
        )
    }

    fn runtime_tree_is_exact(root: &Path, expected: &ExpectedRuntimeTree) -> bool {
        let java = root.join(runtime_java_relative_path());
        fs::read(java).is_ok_and(|bytes| bytes == expected.java_bytes)
            && fs::read(root.join("lib/jvm.cfg")).is_ok_and(|bytes| bytes == expected.config_bytes)
            && fs::read(root.join(".axial-runtime-manifest.json"))
                .is_ok_and(|bytes| bytes == expected.proof_bytes)
            && fs::read(root.join(".axial-ready")).is_ok_and(|bytes| bytes == b"ready")
            && !root.join("sentinel").exists()
            && !root.with_file_name("java-runtime-delta.staging").exists()
            && !root
                .with_file_name("java-runtime-delta.quarantine")
                .exists()
    }

    fn runtime_shell_is_unchanged(root: &Path) -> bool {
        fs::read(root.join("sentinel")).is_ok_and(|bytes| bytes == b"canonical")
            && !root.join(runtime_java_relative_path()).exists()
            && !root.join(".axial-runtime-manifest.json").exists()
            && !root.join(".axial-ready").exists()
            && !root.with_file_name("java-runtime-delta.staging").exists()
            && !root
                .with_file_name("java-runtime-delta.quarantine")
                .exists()
    }

    fn prepare_runtime_shell(cache: &ManagedRuntimeCache) -> (RuntimeId, std::path::PathBuf) {
        let component = RuntimeId::from("java-runtime-delta");
        let root = cache
            .component_root(component.as_str())
            .expect("managed runtime root");
        fs::create_dir(&root).expect("canonical runtime shell");
        fs::write(root.join("sentinel"), b"canonical").expect("canonical sentinel");
        (component, root)
    }

    #[tokio::test]
    async fn sibling_failure_cancels_and_drains_open_runtime_ownership() {
        let (cancellation, mut task_control) = runtime_materialization_control();
        let (cleaned_tx, cleaned_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            task_control.cancelled().await;
            let _ = cleaned_tx.send(());
            RuntimeEnsureTaskOutcome::Cancelled
        });
        let pipeline = controlled_pipeline(cancellation, task);
        let primary_error = DownloadError::ResolveManifest("artifact failed".to_string());

        let result = timeout(
            Duration::from_secs(1),
            settle_runtime_pipeline_after_failure(Some(pipeline), primary_error),
        )
        .await
        .expect("open runtime cancellation should drain");

        cleaned_rx
            .await
            .expect("runtime task should finish cancellation cleanup");
        assert!(matches!(
            result,
            DownloadError::ResolveManifest(message) if message == "artifact failed"
        ));
    }

    #[tokio::test]
    async fn sibling_failure_awaits_runtime_after_settlement_claim_wins() {
        let (cancellation, task_control) = runtime_materialization_control();
        assert!(task_control.claim_publication_settlement());
        assert_eq!(
            cancellation.cancel_before_publication(),
            RuntimeMaterializationCancellation::SettlementRequired
        );
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = started_tx.send(());
            let _ = release_rx.await;
            assert!(task_control.finish());
            RuntimeEnsureTaskOutcome::Complete(Err(JavaRuntimeLookupError::Install(
                "runtime settlement failed".to_string(),
            )))
        });
        let pipeline = controlled_pipeline(cancellation, task);
        started_rx.await.expect("settlement task should start");
        let primary_error = DownloadError::ResolveManifest("artifact failed".to_string());
        let mut settlement = tokio::spawn(async move {
            settle_runtime_pipeline_after_failure(Some(pipeline), primary_error).await
        });

        assert!(
            timeout(Duration::from_millis(25), &mut settlement)
                .await
                .is_err()
        );
        release_tx.send(()).expect("release runtime settlement");
        let result = timeout(Duration::from_secs(1), settlement)
            .await
            .expect("runtime settlement should terminate")
            .expect("runtime settlement task");

        assert!(matches!(
            result,
            DownloadError::ResolveManifest(message) if message == "artifact failed"
        ));
    }

    #[tokio::test]
    async fn dropping_open_runtime_pipeline_transfers_cleanup_to_monitor() {
        let (cancellation, mut task_control) = runtime_materialization_control();
        let (cleaned_tx, cleaned_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            task_control.cancelled().await;
            let _ = cleaned_tx.send(());
            RuntimeEnsureTaskOutcome::Cancelled
        });

        drop(controlled_pipeline(cancellation, task));

        timeout(Duration::from_secs(1), cleaned_rx)
            .await
            .expect("detached runtime monitor should retain the task")
            .expect("runtime task should observe cancellation");
    }

    #[tokio::test]
    async fn real_pipeline_cancellation_before_publication_claim_preserves_canonical_state() {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let (component, root) = prepare_runtime_shell(&cache);
        let mut gate = block_runtime_before_publication_claim_for_test(&root);
        let (pipeline, _) = runtime_pipeline_fixture(cache.clone()).await;

        timeout(Duration::from_secs(2), gate.wait_until_reached())
            .await
            .expect("runtime stage should reach the publication claim");
        assert!(root.with_file_name("java-runtime-delta.staging").is_dir());
        assert_eq!(fs::read(root.join("sentinel")).unwrap(), b"canonical");
        assert!(!runtime_publication_locks_available_for_test(
            &cache, &component
        ));
        assert_eq!(
            pipeline.cancellation.cancel_before_publication(),
            RuntimeMaterializationCancellation::Cancelled
        );
        gate.release();

        let error = timeout(
            Duration::from_secs(2),
            settle_runtime_pipeline_after_failure(
                Some(pipeline),
                DownloadError::ResolveManifest("artifact failed".to_string()),
            ),
        )
        .await
        .expect("pre-claim cancellation should drain");

        assert!(matches!(
            error,
            DownloadError::ResolveManifest(message) if message == "artifact failed"
        ));
        assert!(runtime_shell_is_unchanged(&root));
        assert!(runtime_publication_locks_available_for_test(
            &cache, &component
        ));
    }

    #[tokio::test]
    async fn real_pipeline_sibling_failure_waits_after_publication_claim() {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let (component, root) = prepare_runtime_shell(&cache);
        let mut gate = block_runtime_publication_for_test(&root);
        let (pipeline, expected) = runtime_pipeline_fixture(cache.clone()).await;

        timeout(Duration::from_secs(2), gate.wait_until_reached())
            .await
            .expect("runtime task should enter publication");
        assert_eq!(
            pipeline.cancellation.cancel_before_publication(),
            RuntimeMaterializationCancellation::SettlementRequired
        );
        let mut settlement = tokio::spawn(async move {
            settle_runtime_pipeline_after_failure(
                Some(pipeline),
                DownloadError::ResolveManifest("artifact failed".to_string()),
            )
            .await
        });
        assert!(
            timeout(Duration::from_millis(25), &mut settlement)
                .await
                .is_err(),
            "sibling failure must wait for claimed runtime publication"
        );
        assert!(!runtime_publication_locks_available_for_test(
            &cache, &component
        ));
        gate.release();

        let error = timeout(Duration::from_secs(2), settlement)
            .await
            .expect("runtime publication should settle")
            .expect("runtime settlement task");
        assert!(matches!(
            error,
            DownloadError::ResolveManifest(message) if message == "artifact failed"
        ));
        assert!(runtime_tree_is_exact(&root, &expected));
        assert!(runtime_publication_locks_available_for_test(
            &cache, &component
        ));
    }

    #[tokio::test]
    async fn dropping_real_pipeline_after_publication_claim_monitors_settlement() {
        let cache = ManagedRuntimeCache::isolated_for_test().expect("runtime cache");
        let (component, root) = prepare_runtime_shell(&cache);
        let mut gate = block_runtime_publication_for_test(&root);
        let (pipeline, expected) = runtime_pipeline_fixture(cache.clone()).await;

        timeout(Duration::from_secs(2), gate.wait_until_reached())
            .await
            .expect("runtime task should enter publication");
        assert_eq!(
            pipeline.cancellation.cancel_before_publication(),
            RuntimeMaterializationCancellation::SettlementRequired
        );
        drop(pipeline);
        assert!(!runtime_publication_locks_available_for_test(
            &cache, &component
        ));
        gate.release();

        timeout(Duration::from_secs(2), async {
            loop {
                if runtime_tree_is_exact(&root, &expected)
                    && runtime_publication_locks_available_for_test(&cache, &component)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached runtime monitor should settle publication");
    }

    #[test]
    fn unsupported_runtime_platform_maps_to_typed_download_error() {
        let error =
            runtime_lookup_error_to_download_error(JavaRuntimeLookupError::UnsupportedPlatform {
                component: "jre-legacy".to_string(),
                platform: "mac-os-arm64".to_string(),
            });

        assert!(matches!(
            error,
            DownloadError::RuntimeUnavailableForPlatform {
                component,
                platform
            } if component == "jre-legacy" && platform == "mac-os-arm64"
        ));
    }

    #[test]
    fn rosetta_required_runtime_maps_to_typed_download_error() {
        let error =
            runtime_lookup_error_to_download_error(JavaRuntimeLookupError::RosettaRequired {
                component: "jre-legacy".to_string(),
            });

        assert!(matches!(
            error,
            DownloadError::RuntimeRosettaRequired { component }
                if component == "jre-legacy"
        ));
    }

    #[test]
    fn other_runtime_errors_stay_prepare_runtime_errors() {
        let error = runtime_lookup_error_to_download_error(JavaRuntimeLookupError::Install(
            "network failed".to_string(),
        ));

        assert!(matches!(
            error,
            DownloadError::PrepareRuntime(message) if message == "failed to install java runtime: network failed"
        ));
    }

    #[test]
    fn runtime_source_errors_map_to_typed_download_errors() {
        let failure = crate::runtime::RuntimeSourceFailure::new(
            crate::runtime::RuntimeId::from("java-runtime-gamma"),
            crate::runtime::RuntimeSourceFailureKind::Unavailable,
            "provider unavailable",
        );
        let error = runtime_lookup_error_to_download_error(JavaRuntimeLookupError::RuntimeSource(
            failure.clone(),
        ));

        assert!(matches!(
            error,
            DownloadError::RuntimeSource(actual) if actual == failure
        ));
    }
}
