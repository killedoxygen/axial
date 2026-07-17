use super::model::{DownloadError, DownloadProgress, progress};
use super::plan::TransferPlan;
use crate::launch::JavaVersion;
use crate::runtime::{
    JavaRuntimeLookupError, ManagedRuntimeCache, RuntimeEnsureEvent, RuntimeSourceReceipt,
    materialize_preferred_runtime_source,
};
use std::sync::Arc;
use tokio::sync::mpsc;

pub(super) struct RuntimeEnsurePipeline {
    pub(super) task:
        tokio::task::JoinHandle<Result<Option<RuntimeSourceReceipt>, JavaRuntimeLookupError>>,
    pub(super) progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
}

pub(super) fn spawn_runtime_ensure_pipeline(
    runtime_cache: ManagedRuntimeCache,
    java_version: JavaVersion,
    source_receipt: RuntimeSourceReceipt,
    plan: Arc<TransferPlan>,
) -> RuntimeEnsurePipeline {
    // Runtime bytes are unknown until the component manifest is fetched (and
    // zero when a cached runtime resolves); reserve the contribution so
    // partial totals are not stamped as near-complete in the meantime.
    plan.expect_contribution();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let event_java_version = java_version.clone();
        let progress_tx = progress_tx.clone();
        let mut plan_contribution_resolved = false;
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
                        if !plan_contribution_resolved && *bytes_total > 0 {
                            plan.resolve_contribution(*bytes_total);
                            plan_contribution_resolved = true;
                        }
                        if *bytes_done > plan_done_seen {
                            plan.add_done(*bytes_done - plan_done_seen);
                            plan_done_seen = *bytes_done;
                        }
                    }
                    RuntimeEnsureEvent::ManagedRuntimeReady { .. } => {
                        if !plan_contribution_resolved {
                            plan.resolve_contribution(0);
                            plan_contribution_resolved = true;
                        }
                    }
                    RuntimeEnsureEvent::DownloadingManagedRuntime { .. } => {}
                }
                let _ = progress_tx.send(runtime_ensure_progress(&event_java_version, event));
            },
        )
        .await;
        if !plan_contribution_resolved {
            plan.resolve_contribution(0);
        }
        Ok::<_, JavaRuntimeLookupError>(Some(source_receipt?))
    });

    RuntimeEnsurePipeline { task, progress_rx }
}

#[cfg(test)]
pub(super) fn spawn_test_runtime_source_pipeline(
    source_receipt: RuntimeSourceReceipt,
    plan: Arc<TransferPlan>,
) -> RuntimeEnsurePipeline {
    plan.expect_contribution();
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        plan.resolve_contribution(0);
        drop(progress_tx);
        Ok(Some(source_receipt))
    });

    RuntimeEnsurePipeline { task, progress_rx }
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

pub(super) async fn recv_runtime_progress(
    pipeline: &mut Option<RuntimeEnsurePipeline>,
) -> Option<DownloadProgress> {
    pipeline.as_mut()?.progress_rx.recv().await
}

pub(super) async fn finish_runtime_pipeline_after_artifacts<F, T>(
    pipeline: Option<RuntimeEnsurePipeline>,
    artifact_result: Result<T, DownloadError>,
    send: &mut F,
) -> Result<(Option<RuntimeSourceReceipt>, T), DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let Some(RuntimeEnsurePipeline {
        mut task,
        mut progress_rx,
    }) = pipeline
    else {
        return artifact_result.map(|artifacts| (None, artifacts));
    };

    match artifact_result {
        Err(error) => {
            task.abort();
            let _ = task.await;
            Err(error)
        }
        Ok(artifacts) => {
            let mut progress_open = true;
            loop {
                tokio::select! {
                    progress = progress_rx.recv(), if progress_open => {
                        if let Some(progress) = progress {
                            send(progress);
                        } else {
                            progress_open = false;
                        }
                    }
                    result = &mut task => {
                        while let Ok(progress) = progress_rx.try_recv() {
                            send(progress);
                        }
                        let runtime_result = match result {
                            Ok(result) => result.map_err(runtime_lookup_error_to_download_error),
                            Err(error) => Err(DownloadError::PrepareRuntime(error.to_string())),
                        };
                        return runtime_result.map(|receipt| (receipt, artifacts));
                    }
                }
            }
        }
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
