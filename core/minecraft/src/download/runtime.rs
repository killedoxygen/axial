use super::model::{DownloadError, DownloadProgress, progress};
use crate::launch::JavaVersion;
use crate::runtime::{RuntimeEnsureEvent, ensure_runtime_with_events};
use std::path::PathBuf;
use tokio::sync::mpsc;

pub(super) struct RuntimeEnsurePipeline {
    pub(super) task: tokio::task::JoinHandle<Result<JavaVersion, String>>,
    pub(super) progress_rx: mpsc::UnboundedReceiver<DownloadProgress>,
}

pub(super) fn spawn_runtime_ensure_pipeline(
    mc_dir: PathBuf,
    java_version: JavaVersion,
) -> RuntimeEnsurePipeline {
    let (progress_tx, progress_rx) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let event_java_version = java_version.clone();
        let progress_tx = progress_tx.clone();
        ensure_runtime_with_events(&mc_dir, &java_version, "", false, |event| {
            let _ = progress_tx.send(runtime_ensure_progress(&event_java_version, event));
        })
        .await
        .map_err(|error| error.to_string())?;
        Ok::<_, String>(java_version)
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
        RuntimeEnsureEvent::InstallingManagedRuntimeFiles {
            component,
            current,
            total,
            file,
        } => {
            let detail = match file {
                Some(file) if current > 0 && total > 0 => {
                    format!("Runtime files ({current}/{total}): {file}")
                }
                Some(file) => file,
                None if total > 0 => format!("Runtime files ({current}/{total})"),
                None => format!(
                    "Installing {} (Java {})",
                    runtime_component_label(&component),
                    java_version.major_version
                ),
            };
            progress(
                "java_runtime",
                bounded_progress_count(current),
                bounded_progress_count(total),
                Some(detail),
            )
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

pub(super) async fn finish_runtime_pipeline_after_artifacts<F>(
    pipeline: Option<RuntimeEnsurePipeline>,
    artifact_result: Result<(), DownloadError>,
    send: &mut F,
) -> Result<Option<JavaVersion>, DownloadError>
where
    F: FnMut(DownloadProgress),
{
    let Some(RuntimeEnsurePipeline {
        mut task,
        mut progress_rx,
    }) = pipeline
    else {
        return artifact_result.map(|_| None);
    };

    match artifact_result {
        Err(error) => {
            task.abort();
            Err(error)
        }
        Ok(()) => {
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
                            Ok(result) => result.map_err(DownloadError::PrepareRuntime),
                            Err(error) => Err(DownloadError::PrepareRuntime(error.to_string())),
                        };
                        return runtime_result.map(Some);
                    }
                }
            }
        }
    }
}
