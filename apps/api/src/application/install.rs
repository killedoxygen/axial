//! Application-owned install orchestration facade.
//!
//! The facade owns request/response contracts, vanilla install worker
//! coordination, and status composition. Child modules own loader workflows,
//! operation journal/progress mapping, Guardian repair mapping, and event
//! streaming. Core Minecraft code still owns provider resolution, download
//! verification, and concrete install effects.

mod loader;
mod model;
mod operation;
mod repair;
mod stream;

use super::InstallVersionCommand;
use crate::application::instances::{
    instance_version_is_installed_and_launchable, invalidate_create_view_installed_scan,
};
use crate::guardian::{GuardianArtifactRepairOutcome, GuardianArtifactRepairStatus};
use crate::observability::{
    operation_journal_proof_record,
    telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
    },
};
use crate::state::AppState;
use crate::state::contracts::OperationId;
use crate::state::{
    ActiveQueuedInstallEntry, ContentQueueAction, InstallQueueEnqueueOutcome,
    InstallQueuePlacement, InstallQueueSnapshot, InstallQueueSpec, InstallStore,
    OperationJournalStore, QueuedContentSelection, QueuedInstallEntry, SetupInstanceBaseline,
    SetupInstanceCleanup, SetupInstancePathKind, SetupInstancePathSnapshot,
};
use axial_config::{INSTANCE_LAYOUT_DIRS, Instance, SHARED_INSTANCE_FILES};
use axial_minecraft::{
    DownloadError, DownloadProgress, Downloader, LoaderComponentId,
    download::{ExecutionDownloadFact, SelectedDownloadArtifactDescriptor},
    resolve_build_record,
};
use axum::{Json, http::StatusCode};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;

pub(crate) const INSTALL_FAILURE_MESSAGE: &str =
    "Install failed. Check your connection and app data permissions, then try again.";
pub(crate) const LOADER_INSTALL_INTERRUPTED_MESSAGE: &str =
    "Loader install stopped before completing. Try again.";
pub(crate) const BASE_INSTALL_FAILED_MESSAGE: &str =
    "Base game install failed. Retry the install from Downloads.";
const INSTALL_REPAIR_RESUME_MAX_DEPTH: u8 = 1;
const CONTENT_INSTANCE_REMOVED_PHASE: &str = "error_instance_removed";

pub type InstallApplicationError = (StatusCode, Json<serde_json::Value>);

use loader::start_loader_install;
#[cfg(test)]
use loader::{
    base_install_failed_progress, loader_error_progress, loader_install_done_progress,
    loader_install_key_fields, wait_for_active_vanilla_base_install,
};
pub use loader::{loader_builds, loader_components, loader_error_response, loader_game_versions};
pub use model::{
    InstallActionViewModel, InstallFailureViewModel, InstallGuardianOutcomeSummary,
    InstallGuardianRepairSummary, InstallProgressStepViewModel, InstallProgressViewModel,
    InstallQueueActiveViewModel, InstallQueueContentActionRequest,
    InstallQueueContentItemViewModel, InstallQueueContentSelection,
    InstallQueueInstallItemViewModel, InstallQueueLoaderItemViewModel, InstallQueueNoticeViewModel,
    InstallQueueRequest, InstallQueueStateResponse, InstallQueueViewModel,
    InstallQueuedItemViewModel, InstallStartResponse, InstallStatusResponse, InstallVersionStaging,
    InstallVersionStartRequest, LoaderBuildsRequest, LoaderInstallStartRequest,
};
use operation::{
    InstallProgressCoalescer, install_failure_point_from_journal, install_journal_is_terminal,
    install_progress_history_from_journal, install_progress_record,
    install_progress_with_terminal_error, install_repair_facts_from_download_error_or_facts,
    interrupted_install_progress, observed_install_failure_progress, public_install_id,
};
pub use operation::{
    begin_install_operation_journal, install_guardian_outcome_summary_from_journal,
    install_operation_id, loader_install_progress_view_model, public_loader_install_progress_json,
    public_vanilla_install_progress_json, record_install_operation_guardian_evidence,
    record_install_operation_guardian_failure_outcome,
    record_install_operation_guardian_failure_outcome_for_error_with_memory,
    record_install_operation_guardian_failure_outcome_with_memory,
    record_install_operation_interrupted, record_install_operation_progress,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, sanitize_install_progress,
    stage_install_version_command, vanilla_install_progress_view_model,
};
pub use repair::{
    install_guardian_repair_summary_from_journal, record_install_operation_guardian_repair_outcome,
    repair_install_artifact_corruption_with_guardian,
};
pub use stream::{install_events_stream, loader_install_events_stream};

pub(crate) async fn start_install_version(
    state: &AppState,
    request: InstallVersionStartRequest,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let (version_id, manifest_url) = effective_install_fields(&request);
    if version_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "version_id is required" })),
        ));
    }

    let mc_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )
    })?;

    let install_id = generate_install_id("install");
    let (install_id, inserted) = state
        .installs()
        .insert_or_existing_active(install_id, version_id.clone(), manifest_url.clone())
        .await;
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: version_id.clone(),
            manifest_url: (!manifest_url.is_empty()).then_some(manifest_url.clone()),
        },
        install_id.clone(),
        operation_id.clone(),
    );
    if !inserted {
        return Ok(InstallStartResponse {
            install_id,
            operation_id,
            view_model: InstallProgressViewModel::starting(),
        });
    }
    begin_install_operation_journal(state.journals(), &operation_id, &version_id);

    let store = state.installs().clone();
    let journals = state.journals().clone();
    let failure_memory = state.failure_memory().clone();
    let telemetry = state.telemetry().clone();
    let mc_dir = PathBuf::from(mc_dir);
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_failure_memory = failure_memory.clone();
    let worker_operation_id = operation_id_task.clone();
    let worker_telemetry = telemetry.clone();
    InstallStore::spawn_tracked_worker_with_interrupt_handler(
        store,
        install_id_task,
        interrupted_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let terminal_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                tokio::spawn(async move {
                    let mut coalescer = InstallProgressCoalescer::default();
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        for progress in coalescer.push(progress) {
                            record_and_emit_install_progress(
                                store.as_ref(),
                                journals.as_ref(),
                                &operation_id,
                                &install_id,
                                progress,
                                &mut last_journal_phase,
                            )
                            .await;
                        }
                    }
                    if let Some(progress) = coalescer.flush() {
                        record_and_emit_install_progress(
                            store.as_ref(),
                            journals.as_ref(),
                            &operation_id,
                            &install_id,
                            progress,
                            &mut last_journal_phase,
                        )
                        .await;
                    }
                })
            };

            let downloader = Downloader::new(mc_dir);
            let mut repair_resume_depth = 0_u8;
            let (final_install_succeeded, final_terminal_progress) = loop {
                if let Ok(mut terminal_progress) = terminal_progress.lock() {
                    *terminal_progress = None;
                }
                let progress_tx_for_downloader = progress_tx.clone();
                let terminal_progress_for_downloader = Arc::clone(&terminal_progress);
                let mut install_facts = Vec::new();
                let mut install_descriptors = Vec::new();
                let install_result = downloader
                    .install_version_with_facts_and_descriptors(
                        &version_id,
                        (!manifest_url.is_empty()).then_some(manifest_url.as_str()),
                        move |progress| {
                            if progress.done {
                                if let Ok(mut terminal_progress) =
                                    terminal_progress_for_downloader.lock()
                                {
                                    *terminal_progress = Some(progress);
                                }
                                return;
                            }
                            let _ = progress_tx_for_downloader.send(progress);
                        },
                        |fact| install_facts.push(fact),
                        |descriptor| install_descriptors.push(descriptor),
                    )
                    .await;
                let attempt_terminal_progress = terminal_progress
                    .lock()
                    .ok()
                    .and_then(|mut progress| progress.take());
                let install_error = match install_result {
                    Ok(()) => break (true, attempt_terminal_progress),
                    Err(error) => error,
                };
                tracing::warn!(
                    operation_id = worker_operation_id.as_str(),
                    version_id = version_id.as_str(),
                    failure_kind = install_error_log_kind(&install_error),
                    "install worker observed failed install"
                );
                let observed_at = chrono::Utc::now().to_rfc3339();
                let repair_outcome = record_install_failure_outcome_and_repair_for_error(
                    worker_journals.as_ref(),
                    worker_failure_memory.as_ref(),
                    &worker_operation_id,
                    &install_error,
                    &install_facts,
                    &install_descriptors,
                    &observed_at,
                )
                .await;
                if repair_resume_depth < INSTALL_REPAIR_RESUME_MAX_DEPTH
                    && repair_outcome.as_ref().is_some_and(|outcome| {
                        outcome.status == GuardianArtifactRepairStatus::Repaired
                    })
                {
                    repair_resume_depth += 1;
                    continue;
                }
                break (
                    false,
                    Some(install_progress_with_terminal_error(
                        terminal_failure_progress_or_default(attempt_terminal_progress),
                        &install_error,
                    )),
                );
            };
            let terminal_progress = if final_install_succeeded {
                final_terminal_progress.unwrap_or_else(vanilla_install_done_progress)
            } else {
                final_terminal_progress.unwrap_or_else(observed_install_failure_progress)
            };
            if !final_install_succeeded {
                let sanitized = sanitize_install_progress(terminal_progress.clone());
                emit_install_failed(
                    worker_telemetry.as_ref(),
                    sanitized
                        .error
                        .as_deref()
                        .unwrap_or(INSTALL_FAILURE_MESSAGE),
                );
            }
            let _ = progress_tx.send(terminal_progress);
            drop(progress_tx);
            let _ = store_task.await;
        },
        move |progress| {
            record_install_operation_interrupted(journals.as_ref(), &operation_id_task, &progress);
        },
    );

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
        view_model: InstallProgressViewModel::starting(),
    })
}

pub(super) async fn record_install_failure_outcome_and_repair(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    install_facts: &[ExecutionDownloadFact],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    record_install_operation_guardian_evidence(journals, operation_id, install_facts);
    record_install_operation_guardian_failure_outcome_with_memory(
        journals,
        failure_memory,
        operation_id,
        install_facts,
        observed_at,
    );
    repair_install_failure_with_guardian(
        journals,
        failure_memory,
        operation_id,
        install_facts,
        install_descriptors,
        observed_at,
    )
    .await
}

pub(super) async fn record_install_failure_outcome_and_repair_for_error(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    error: &DownloadError,
    install_facts: &[ExecutionDownloadFact],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    record_install_operation_guardian_evidence(journals, operation_id, install_facts);
    record_install_operation_guardian_failure_outcome_for_error_with_memory(
        journals,
        failure_memory,
        operation_id,
        error,
        install_facts,
        observed_at,
    );
    let repair_facts = install_repair_facts_from_download_error_or_facts(error, install_facts);
    repair_install_failure_with_guardian(
        journals,
        failure_memory,
        operation_id,
        &repair_facts,
        install_descriptors,
        observed_at,
    )
    .await
}

fn install_error_log_kind(error: &DownloadError) -> &'static str {
    match error {
        DownloadError::FileOperation(_) => "file_operation",
        DownloadError::ResolveManifest(_) => "resolve_manifest",
        DownloadError::Request(_) => "request",
        DownloadError::ParseVersion(_) => "parse_version",
        DownloadError::PrepareRuntime(_) => "prepare_runtime",
        DownloadError::RuntimeUnavailableForPlatform { .. } => "runtime_unavailable_for_platform",
        DownloadError::RuntimeRosettaRequired { .. } => "runtime_rosetta_required",
        DownloadError::Integrity(_) => "integrity",
    }
}

async fn repair_install_failure_with_guardian(
    journals: &crate::state::OperationJournalStore,
    failure_memory: &crate::state::GuardianFailureMemoryStore,
    operation_id: &crate::state::contracts::OperationId,
    install_facts: &[ExecutionDownloadFact],
    install_descriptors: &[SelectedDownloadArtifactDescriptor],
    observed_at: &str,
) -> Option<GuardianArtifactRepairOutcome> {
    let repair_client = reqwest::Client::new();
    let repair_outcome = repair_install_artifact_corruption_with_guardian(
        journals,
        failure_memory,
        &repair_client,
        operation_id,
        install_facts,
        install_descriptors,
        observed_at,
    )
    .await;
    if let Some(repair_outcome) = repair_outcome.as_ref() {
        record_install_operation_guardian_repair_outcome(journals, operation_id, repair_outcome);
    }
    repair_outcome
}

fn terminal_failure_progress_or_default(progress: Option<DownloadProgress>) -> DownloadProgress {
    progress
        .filter(|progress| progress.error.is_some())
        .unwrap_or_else(observed_install_failure_progress)
}

fn emit_install_failed(telemetry: &TelemetryHub, summary: &str) {
    telemetry.emit(TelemetryEvent::error_captured(
        TelemetryErrorKind::InstallFailed,
        TelemetryErrorArea::Install,
        TelemetryErrorLevel::Error,
        summary,
    ));
}

async fn record_and_emit_install_progress(
    store: &InstallStore,
    journals: &OperationJournalStore,
    operation_id: &OperationId,
    install_id: &str,
    progress: DownloadProgress,
    last_journal_phase: &mut Option<String>,
) {
    record_install_operation_progress(journals, operation_id, &progress, last_journal_phase);
    store
        .emit_record(install_id, install_progress_record(progress))
        .await;
}

fn vanilla_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub async fn install_status(
    state: &AppState,
    id: &str,
) -> Result<InstallStatusResponse, InstallApplicationError> {
    let operation_id = install_operation_id(id);
    let snapshot = state.installs().snapshot(id).await;
    let journal = state.journals().get(&operation_id);
    if snapshot.is_none() && journal.is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "install session not found" })),
        ));
    }

    let done = snapshot.as_ref().is_some_and(|snapshot| snapshot.done)
        || journal
            .as_ref()
            .is_some_and(|journal| install_journal_is_terminal(journal.status));
    let progress = snapshot
        .as_ref()
        .and_then(|snapshot| snapshot.latest.as_ref())
        .map(|record| vec![record.progress.clone()])
        .or_else(|| journal.as_ref().map(install_progress_history_from_journal))
        .unwrap_or_default()
        .into_iter()
        .map(sanitize_install_progress)
        .collect::<Vec<_>>();
    let view_model = progress
        .last()
        .map(vanilla_install_progress_view_model)
        .unwrap_or_else(InstallProgressViewModel::starting);
    let failure_point = journal
        .as_ref()
        .and_then(install_failure_point_from_journal);
    let guardian_repair = journal
        .as_ref()
        .and_then(install_guardian_repair_summary_from_journal);
    let guardian = journal
        .as_ref()
        .and_then(install_guardian_outcome_summary_from_journal);
    let failure_view_model =
        install_failure_view_model(&view_model, guardian.as_ref(), guardian_repair.as_ref());
    let proof = journal
        .as_ref()
        .filter(|journal| install_journal_is_terminal(journal.status))
        .map(operation_journal_proof_record);

    Ok(InstallStatusResponse {
        install_id: public_install_id(id),
        operation_id,
        done,
        progress,
        view_model,
        failure_view_model,
        failure_point,
        guardian,
        guardian_repair,
        proof,
    })
}

pub async fn install_queue_status(
    state: &AppState,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let started_install = maybe_start_next_queued_install(state).await?;
    let response = install_queue_state_response(state, None, started_install.clone()).await;
    spawn_install_queue_monitor_for_started(state.clone(), started_install.as_ref());
    Ok(response)
}

pub async fn enqueue_install(
    state: &AppState,
    request: InstallQueueRequest,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_dependency(state, request, None, None).await
}

pub(crate) async fn enqueue_install_with_dependency(
    state: &AppState,
    request: InstallQueueRequest,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(
        state,
        request,
        InstallQueuePlacement::Back,
        prerequisite_queue_id,
        setup_cleanup,
    )
    .await
}

pub async fn retry_install(
    state: &AppState,
    request: InstallQueueRequest,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(state, request, InstallQueuePlacement::Front, None, None).await
}

pub async fn remove_queued_install(
    state: &AppState,
    queue_id: &str,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let removed = state.installs().remove_queued_install(queue_id).await;
    let removed_instance_id = if let Some(QueuedInstallEntry {
        spec:
            InstallQueueSpec::Content {
                instance_id,
                action,
                ..
            },
        ..
    }) = removed.as_ref()
        && content_action_setup_cleanup(action).is_some()
    {
        remove_pristine_setup_instance(
            state,
            instance_id,
            content_action_setup_cleanup(action).expect("setup cleanup is present"),
        )
        .await
        .then(|| instance_id.clone())
    } else {
        None
    };
    let notice = removed
        .as_ref()
        .map(|entry| {
            install_queue_notice(
                "removed",
                "info",
                "Removed from queue",
                Some(install_queue_label(&entry.spec)),
            )
        })
        .or_else(|| {
            Some(install_queue_notice(
                "remove_unavailable",
                "warn",
                "Queued install was not removed",
                Some("It may have already started or left the queue.".to_string()),
            ))
        });
    let mut response = install_queue_state_response(state, notice, None).await;
    response.removed_instance_id = removed_instance_id;
    Ok(response)
}

fn content_action_owns_instance(action: &ContentQueueAction) -> bool {
    content_action_setup_cleanup(action).is_some()
}

fn content_action_setup_cleanup(action: &ContentQueueAction) -> Option<&SetupInstanceCleanup> {
    match action {
        ContentQueueAction::Install { setup_cleanup, .. }
        | ContentQueueAction::Modpack { setup_cleanup, .. } => setup_cleanup.as_ref(),
        ContentQueueAction::Uninstall { .. } => None,
    }
}

pub(crate) fn setup_instance_cleanup(
    state: &AppState,
    instance: &Instance,
    seed_shared_files: bool,
) -> SetupInstanceCleanup {
    let baseline = setup_instance_baseline(state, instance, seed_shared_files)
        .filter(|baseline| setup_instance_matches_baseline(state, baseline))
        .map(Box::new);
    SetupInstanceCleanup { baseline }
}

fn setup_instance_baseline(
    state: &AppState,
    instance: &Instance,
    seed_shared_files: bool,
) -> Option<SetupInstanceBaseline> {
    let mut paths = INSTANCE_LAYOUT_DIRS
        .iter()
        .map(|path| SetupInstancePathSnapshot {
            relative_path: PathBuf::from(path),
            kind: SetupInstancePathKind::Directory,
        })
        .collect::<Vec<_>>();
    if seed_shared_files && let Some(library_dir) = state.library_dir() {
        for file_name in SHARED_INSTANCE_FILES {
            let source = Path::new(&library_dir).join(file_name);
            let metadata = match fs::symlink_metadata(&source) {
                Ok(metadata) if metadata.is_file() => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                _ => return None,
            };
            paths.push(SetupInstancePathSnapshot {
                relative_path: PathBuf::from(file_name),
                kind: SetupInstancePathKind::File {
                    size: metadata.len(),
                    sha512: axial_content::sha512_file(&source).ok()?,
                },
            });
        }
    }
    paths.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
    Some(SetupInstanceBaseline {
        instance: instance.clone(),
        paths,
    })
}

fn setup_instance_matches_baseline(state: &AppState, baseline: &SetupInstanceBaseline) -> bool {
    if state.instances().get(&baseline.instance.id).as_ref() != Some(&baseline.instance) {
        return false;
    }
    setup_instance_paths_match(
        &state.instances().game_dir(&baseline.instance.id),
        &baseline.paths,
    )
}

fn setup_instance_paths_match(game_dir: &Path, expected: &[SetupInstancePathSnapshot]) -> bool {
    let Ok(root_metadata) = fs::symlink_metadata(game_dir) else {
        return false;
    };
    if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
        return false;
    }
    let expected: HashMap<&Path, &SetupInstancePathKind> = expected
        .iter()
        .map(|entry| (entry.relative_path.as_path(), &entry.kind))
        .collect();
    let mut seen = HashSet::new();
    let mut pending = vec![game_dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let Ok(entries) = fs::read_dir(&directory) else {
            return false;
        };
        for entry in entries {
            let Ok(entry) = entry else { return false };
            let path = entry.path();
            let Ok(relative) = path.strip_prefix(game_dir) else {
                return false;
            };
            let Some(expected_kind) = expected.get(relative) else {
                return false;
            };
            let Ok(metadata) = fs::symlink_metadata(&path) else {
                return false;
            };
            if metadata.file_type().is_symlink() || !seen.insert(relative.to_path_buf()) {
                return false;
            }
            match expected_kind {
                SetupInstancePathKind::Directory if metadata.is_dir() => pending.push(path),
                SetupInstancePathKind::File { size, sha512 }
                    if metadata.is_file()
                        && metadata.len() == *size
                        && axial_content::sha512_file(&path).ok().as_ref() == Some(sha512) => {}
                _ => return false,
            }
        }
    }
    seen.len() == expected.len()
}

/// Remove an untouched instance created solely for setup, but only while no
/// launch or content mutation can be using it. Any metadata or filesystem
/// difference is treated as user ownership and retains the instance.
pub(crate) async fn remove_pristine_setup_instance(
    state: &AppState,
    instance_id: &str,
    cleanup: &SetupInstanceCleanup,
) -> bool {
    let Some(_lifecycle_guard) = state.sessions().try_lock_instance_lifecycle(instance_id) else {
        return false;
    };
    if state.sessions().has_active_instance(instance_id).await {
        return false;
    }
    let Some(baseline) = cleanup.baseline.as_ref() else {
        return false;
    };
    if baseline.instance.id != instance_id || !setup_instance_matches_baseline(state, baseline) {
        return false;
    }
    state.instances().remove(instance_id, true).is_ok()
}

async fn enqueue_install_with_placement(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let spec =
        install_queue_spec_from_request(state, request, prerequisite_queue_id, setup_cleanup)
            .await?;
    let queue_id = generate_install_id("install-queue");
    let outcome = state
        .installs()
        .enqueue_queued_install(queue_id, spec.clone(), placement)
        .await;
    let notice = Some(install_queue_notice_for_outcome(&outcome, &spec, placement));
    let started_install = maybe_start_next_queued_install(state).await?;
    let response = install_queue_state_response(state, notice, started_install.clone()).await;
    spawn_install_queue_monitor_for_started(state.clone(), started_install.as_ref());
    Ok(response)
}

async fn install_queue_spec_from_request(
    state: &AppState,
    request: InstallQueueRequest,
    prerequisite_queue_id: Option<String>,
    setup_cleanup: Option<SetupInstanceCleanup>,
) -> Result<InstallQueueSpec, InstallApplicationError> {
    match request.kind.trim() {
        "vanilla" | "minecraft" => {
            let (version_id, manifest_url) =
                effective_install_fields(&InstallVersionStartRequest {
                    version_id: request.version_id,
                    manifest_url: request.manifest_url,
                });
            if version_id.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "version_id is required" })),
                ));
            }
            state.library_dir().ok_or_else(|| {
                (
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({ "error": "Axial library is not configured" })),
                )
            })?;
            Ok(InstallQueueSpec::vanilla(version_id, manifest_url))
        }
        "loader" => {
            let component_id =
                LoaderComponentId::parse(request.component_id.trim()).ok_or_else(|| {
                    (
                        StatusCode::NOT_FOUND,
                        Json(serde_json::json!({ "error": "unknown loader component" })),
                    )
                })?;
            let build_id = request.build_id.trim().to_string();
            if build_id.is_empty() {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "build_id is required" })),
                ));
            }
            let library_dir = state.library_dir().ok_or_else(|| {
                (
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({ "error": "Axial library is not configured" })),
                )
            })?;
            let build = resolve_build_record(
                PathBuf::from(library_dir).as_path(),
                component_id,
                &build_id,
            )
            .await
            .map_err(loader_error_response)?;
            Ok(InstallQueueSpec::loader(
                build.component_id,
                build.build_id,
                build.version_id,
                build.minecraft_version,
                build.loader_version,
            ))
        }
        "content" => {
            let instance_id = request.instance_id.trim().to_string();
            if instance_id.is_empty() || state.instances().get(&instance_id).is_none() {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(serde_json::json!({ "error": "instance not found" })),
                ));
            }
            let action = match request.content_action.ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({ "error": "content_action is required" })),
                )
            })? {
                InstallQueueContentActionRequest::Install {
                    selections,
                    allow_incompatible,
                } => {
                    if selections.is_empty() || selections.len() > 40 {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "content selections must contain between 1 and 40 items"
                            })),
                        ));
                    }
                    ContentQueueAction::Install {
                        selections: selections
                            .into_iter()
                            .map(|selection| QueuedContentSelection {
                                canonical_id: selection.canonical_id.trim().to_string(),
                                kind: selection.kind,
                                version_id: selection
                                    .version_id
                                    .filter(|value| !value.trim().is_empty()),
                            })
                            .collect(),
                        allow_incompatible,
                        setup_cleanup,
                    }
                }
                InstallQueueContentActionRequest::Uninstall { canonical_ids } => {
                    let canonical_ids = canonical_ids
                        .into_iter()
                        .map(|canonical_id| canonical_id.trim().to_string())
                        .filter(|canonical_id| !canonical_id.is_empty())
                        .collect::<HashSet<_>>();
                    if canonical_ids.is_empty() || canonical_ids.len() > 500 {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": "canonical_ids must contain between 1 and 500 items"
                            })),
                        ));
                    }
                    let mut canonical_ids = canonical_ids.into_iter().collect::<Vec<_>>();
                    canonical_ids.sort();
                    ContentQueueAction::Uninstall { canonical_ids }
                }
                InstallQueueContentActionRequest::Modpack {
                    canonical_id,
                    version_id,
                    selected_paths,
                    include_overrides,
                } => {
                    let canonical_id = canonical_id.trim().to_string();
                    let version_id = version_id.trim().to_string();
                    if canonical_id.is_empty()
                        || version_id.is_empty()
                        || selected_paths.len() > 500
                    {
                        return Err((
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({ "error": "invalid modpack operation" })),
                        ));
                    }
                    ContentQueueAction::Modpack {
                        canonical_id,
                        version_id,
                        selected_paths,
                        include_overrides,
                        setup_cleanup,
                    }
                }
            };
            let label = request.label.trim();
            let label = if label.is_empty() {
                "Instance content".to_string()
            } else {
                label.chars().take(120).collect()
            };
            Ok(InstallQueueSpec::Content {
                instance_id,
                label,
                action,
                prerequisite_queue_id,
            })
        }
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "install kind is required" })),
        )),
    }
}

async fn maybe_start_next_queued_install(
    state: &AppState,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    let entry = loop {
        let Some(entry) = state.installs().reserve_next_queued_install().await else {
            return Ok(None);
        };
        let dependency = match &entry.spec {
            InstallQueueSpec::Content {
                prerequisite_queue_id,
                ..
            } => prerequisite_queue_id.as_deref(),
            _ => None,
        };
        if let Some(dependency) = dependency
            && state.installs().queued_install_succeeded(dependency).await != Some(true)
        {
            state
                .installs()
                .complete_reserved_queued_install(&entry.queue_id, false)
                .await;
            if let InstallQueueSpec::Content {
                instance_id,
                action,
                ..
            } = &entry.spec
                && let Some(cleanup) = content_action_setup_cleanup(action)
            {
                let _ = remove_pristine_setup_instance(state, instance_id, cleanup).await;
            }
            continue;
        }
        break entry;
    };
    let started = match start_queued_install(state, &entry.spec).await {
        Ok(started) => started,
        Err(error) => {
            state
                .installs()
                .complete_reserved_queued_install(&entry.queue_id, false)
                .await;
            return Err(error);
        }
    };
    state
        .installs()
        .mark_queued_install_started(&entry.queue_id, started.install_id.clone())
        .await;
    Ok(Some(started))
}

async fn start_queued_install(
    state: &AppState,
    spec: &InstallQueueSpec,
) -> Result<InstallStartResponse, InstallApplicationError> {
    match spec {
        InstallQueueSpec::Vanilla {
            version_id,
            manifest_url,
        } => {
            start_install_version(
                state,
                InstallVersionStartRequest {
                    version_id: version_id.clone(),
                    manifest_url: manifest_url.clone(),
                },
            )
            .await
        }
        InstallQueueSpec::Loader {
            component_id,
            build_id,
            ..
        } => {
            start_loader_install(
                state,
                LoaderInstallStartRequest {
                    component_id: *component_id,
                    build_id: build_id.clone(),
                },
            )
            .await
        }
        InstallQueueSpec::Content {
            instance_id,
            label,
            action,
            ..
        } => start_content_operation(state, instance_id, label, action).await,
    }
}

async fn start_content_operation(
    state: &AppState,
    instance_id: &str,
    label: &str,
    action: &ContentQueueAction,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let install_id = generate_install_id("content");
    state.installs().insert(install_id.clone()).await;
    let operation_id = install_operation_id(&install_id);
    let worker_state = state.clone();
    let worker_store = state.installs().clone();
    let worker_install_id = install_id.clone();
    let worker_instance_id = instance_id.to_string();
    let worker_action = action.clone();
    let interrupted_state = state.clone();
    let interrupted_instance_id = instance_id.to_string();
    let interrupted_setup_cleanup = content_action_setup_cleanup(action).cloned();
    InstallStore::spawn_tracked_worker_with_async_interrupt_handler(
        state.installs().clone(),
        install_id.clone(),
        content_interrupted_progress(false),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let progress_store = worker_store.clone();
            let progress_install_id = worker_install_id.clone();
            let progress_task = tokio::spawn(async move {
                while let Some(progress) = progress_rx.recv().await {
                    progress_store
                        .emit(&progress_install_id, sanitize_install_progress(progress))
                        .await;
                }
            });

            let result = if content_action_owns_instance(&worker_action)
                && !instance_version_is_installed_and_launchable(&worker_state, &worker_instance_id)
            {
                Err((
                    StatusCode::PRECONDITION_FAILED,
                    Json(serde_json::json!({
                        "error": "Minecraft or the selected mod loader did not finish installing."
                    })),
                ))
            } else {
                match &worker_action {
                    ContentQueueAction::Install {
                        selections,
                        allow_incompatible,
                        ..
                    } => {
                        let request = crate::application::content::ContentInstallRequest {
                            instance_id: worker_instance_id.clone(),
                            selections: selections
                                .iter()
                                .map(|selection| crate::application::content::ContentSelection {
                                    canonical_id: selection.canonical_id.clone(),
                                    kind: selection.kind,
                                    version_id: selection.version_id.clone(),
                                })
                                .collect(),
                            allow_incompatible: *allow_incompatible,
                        };
                        crate::application::content::execute_content_install(
                            &worker_state,
                            request,
                            |progress| {
                                let _ = progress_tx.send(progress);
                            },
                        )
                        .await
                    }
                    ContentQueueAction::Uninstall { canonical_ids } => {
                        let _ = progress_tx.send(content_progress(
                            "removing",
                            0,
                            canonical_ids.len() as i32,
                            false,
                            None,
                        ));
                        crate::application::content::execute_content_uninstalls(
                            &worker_state,
                            &worker_instance_id,
                            canonical_ids,
                        )
                        .await
                    }
                    ContentQueueAction::Modpack {
                        canonical_id,
                        version_id,
                        selected_paths,
                        include_overrides,
                        ..
                    } => crate::application::content::execute_modpack_install(
                        &worker_state,
                        crate::application::content::ModpackInstallRequest {
                            instance_id: worker_instance_id.clone(),
                            canonical_id: canonical_id.clone(),
                            version_id: Some(version_id.clone()),
                            selected_paths: selected_paths.clone(),
                            include_overrides: *include_overrides,
                        },
                        |progress| {
                            let _ = progress_tx.send(progress);
                        },
                    )
                    .await
                    .map(|_| ()),
                }
            };

            let terminal = match result {
                Ok(()) => content_progress("done", 1, 1, true, None),
                Err((_, Json(body))) => {
                    let removed_instance = match content_action_setup_cleanup(&worker_action) {
                        Some(cleanup) => {
                            remove_pristine_setup_instance(
                                &worker_state,
                                &worker_instance_id,
                                cleanup,
                            )
                            .await
                        }
                        None => false,
                    };
                    let mut message = body
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or(INSTALL_FAILURE_MESSAGE)
                        .to_string();
                    if removed_instance {
                        message.push_str(" The incomplete setup instance was removed.");
                    }
                    content_progress(
                        if removed_instance {
                            CONTENT_INSTANCE_REMOVED_PHASE
                        } else {
                            "error"
                        },
                        0,
                        1,
                        true,
                        Some(message),
                    )
                }
            };
            let _ = progress_tx.send(terminal);
            drop(progress_tx);
            let _ = progress_task.await;
        },
        move |_| async move {
            interrupted_content_progress(
                &interrupted_state,
                &interrupted_instance_id,
                interrupted_setup_cleanup.as_ref(),
            )
            .await
        },
    );

    Ok(InstallStartResponse {
        install_id,
        operation_id,
        view_model: InstallProgressViewModel {
            phase_id: "starting".to_string(),
            label: format!("Preparing {label}"),
            progress_pct: 0,
            terminal: false,
            failed: false,
            active_step: None,
        },
    })
}

fn content_progress(
    phase: &str,
    current: i32,
    total: i32,
    done: bool,
    error: Option<String>,
) -> DownloadProgress {
    DownloadProgress {
        phase: phase.to_string(),
        current,
        total,
        file: None,
        error,
        done,
        bytes_done: None,
        bytes_total: None,
    }
}

async fn interrupted_content_progress(
    state: &AppState,
    instance_id: &str,
    setup_cleanup: Option<&SetupInstanceCleanup>,
) -> DownloadProgress {
    let removed = match setup_cleanup {
        Some(cleanup) => remove_pristine_setup_instance(state, instance_id, cleanup).await,
        None => false,
    };
    content_interrupted_progress(removed)
}

fn content_interrupted_progress(removed_instance: bool) -> DownloadProgress {
    content_progress(
        if removed_instance {
            CONTENT_INSTANCE_REMOVED_PHASE
        } else {
            "error"
        },
        0,
        1,
        true,
        Some(if removed_instance {
            "Content operation stopped. The incomplete setup instance was removed.".to_string()
        } else {
            "Content operation stopped before completing. Try again.".to_string()
        }),
    )
}

fn spawn_install_queue_monitor(state: AppState, install_id: String) {
    tokio::spawn(async move {
        let succeeded = wait_for_install_terminal(&state, &install_id).await;
        invalidate_create_view_installed_scan();
        state
            .installs()
            .complete_active_queued_install(&install_id, succeeded)
            .await;
        loop {
            match maybe_start_next_queued_install(&state).await {
                Ok(Some(started_install)) => {
                    spawn_install_queue_monitor(state.clone(), started_install.install_id);
                    break;
                }
                Ok(None) => break,
                Err(_) => continue,
            }
        }
    });
}

fn spawn_install_queue_monitor_for_started(
    state: AppState,
    started_install: Option<&InstallStartResponse>,
) {
    if let Some(started_install) = started_install {
        spawn_install_queue_monitor(state, started_install.install_id.clone());
    }
}

async fn wait_for_install_terminal(state: &AppState, install_id: &str) -> bool {
    let Some((snapshot, mut receiver)) = state.installs().subscribe_records(install_id).await
    else {
        return false;
    };
    if snapshot.done
        || snapshot
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.done)
    {
        return snapshot
            .latest
            .as_ref()
            .is_some_and(|record| record.progress.error.is_none());
    }
    loop {
        match receiver.recv().await {
            Ok(record) if record.progress.done => return record.progress.error.is_none(),
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return false,
        }
    }
}

async fn install_queue_state_response(
    state: &AppState,
    notice: Option<InstallQueueNoticeViewModel>,
    started_install: Option<InstallStartResponse>,
) -> InstallQueueStateResponse {
    let snapshot = state.installs().queue_snapshot().await;
    let active = install_queue_active_view_model(state, snapshot.active.as_ref()).await;
    let items = install_queue_item_view_models(&snapshot);
    let view_model = install_queue_view_model(active.as_ref(), &items);
    InstallQueueStateResponse {
        active,
        items,
        view_model,
        notice,
        started_install,
        removed_instance_id: None,
    }
}

async fn install_queue_active_view_model(
    state: &AppState,
    active: Option<&ActiveQueuedInstallEntry>,
) -> Option<InstallQueueActiveViewModel> {
    let active = active?;
    let install_id = active.install_id.clone();
    let progress = match install_id.as_deref() {
        Some(install_id) => {
            install_queue_active_progress_view_model(state, install_id, &active.spec).await
        }
        None => InstallProgressViewModel::starting(),
    };
    let label = install_queue_label(&active.spec);
    let title = if install_id.is_some() {
        "Installing"
    } else {
        "Starting install"
    };
    let summary = if install_id.is_some() {
        format!("{label} is installing.")
    } else {
        format!("{label} is starting.")
    };
    Some(InstallQueueActiveViewModel {
        queue_id: active.queue_id.clone(),
        operation_id: install_id
            .as_ref()
            .map(|install_id| install_operation_id(install_id)),
        install_id,
        install_started_at_ms: active.install_started_at_ms,
        kind: install_queue_kind(&active.spec).to_string(),
        title: title.to_string(),
        summary,
        label,
        install_item: install_queue_install_item(&active.spec),
        progress,
    })
}

async fn install_queue_active_progress_view_model(
    state: &AppState,
    install_id: &str,
    spec: &InstallQueueSpec,
) -> InstallProgressViewModel {
    let snapshot = state.installs().snapshot(install_id).await;
    let progress = snapshot.and_then(|snapshot| snapshot.latest.map(|record| record.progress));
    let Some(progress) = progress else {
        return InstallProgressViewModel::starting();
    };
    if spec.is_loader() {
        loader_install_progress_view_model(&progress)
    } else {
        vanilla_install_progress_view_model(&progress)
    }
}

fn install_queue_item_view_models(
    snapshot: &InstallQueueSnapshot,
) -> Vec<InstallQueuedItemViewModel> {
    let total = snapshot.pending.len();
    snapshot
        .pending
        .iter()
        .enumerate()
        .map(|(index, entry)| install_queue_item_view_model(entry, index + 1, total))
        .collect()
}

fn install_queue_item_view_model(
    entry: &QueuedInstallEntry,
    position: usize,
    total: usize,
) -> InstallQueuedItemViewModel {
    let label = install_queue_label(&entry.spec);
    InstallQueuedItemViewModel {
        queue_id: entry.queue_id.clone(),
        state_id: "queued".to_string(),
        kind: install_queue_kind(&entry.spec).to_string(),
        title: "Install queued".to_string(),
        summary: if position == 1 {
            format!("{label} is next to start.")
        } else {
            format!("{label} is waiting for earlier downloads.")
        },
        detail: if position == 1 {
            format!("Position 1 of {total}; next to start when the download slot opens.")
        } else {
            let waiting = position.saturating_sub(1);
            format!(
                "Position {position} of {total}; waiting behind {waiting} {}.",
                if waiting == 1 { "item" } else { "items" }
            )
        },
        label,
        position,
        total,
        install_item: install_queue_install_item(&entry.spec),
        remove_action: InstallActionViewModel {
            action: "remove_from_queue".to_string(),
            label: "Remove from queue".to_string(),
            enabled: true,
            disabled_reason: None,
        },
    }
}

fn install_queue_view_model(
    active: Option<&InstallQueueActiveViewModel>,
    items: &[InstallQueuedItemViewModel],
) -> InstallQueueViewModel {
    let queued_count = items.len();
    let queued_count_label = match queued_count {
        0 => "No queued downloads".to_string(),
        1 => "1 queued".to_string(),
        count => format!("{count} queued"),
    };
    let queued_item_label = match queued_count {
        0 => "No items queued".to_string(),
        1 => "1 item queued".to_string(),
        count => format!("{count} items queued"),
    };
    let next_label = items.first().map(|item| item.label.clone());
    let state_id = if active.is_some() {
        "active"
    } else if queued_count > 0 {
        "queued"
    } else {
        "idle"
    };
    let title = if active.is_some() {
        "Downloads active".to_string()
    } else if queued_count > 0 {
        "Downloads queued".to_string()
    } else {
        "Nothing downloading".to_string()
    };
    let summary = if active.is_some() {
        if queued_count > 0 {
            format!("{queued_item_label} behind the active install.")
        } else {
            "No queued downloads behind the active install.".to_string()
        }
    } else if queued_count > 0 {
        format!("{queued_item_label} and waiting to start. The next item will begin automatically.")
    } else {
        "Launch an instance that needs a download, or install a new Minecraft version, and it will show up here."
            .to_string()
    };
    let active_queued_count_label = (queued_count > 0).then(|| format!(", {queued_count_label}"));
    InstallQueueViewModel {
        state_id: state_id.to_string(),
        status_label: if active.is_some() {
            "Installing".to_string()
        } else if queued_count > 0 {
            "Queued".to_string()
        } else {
            "Idle".to_string()
        },
        title,
        summary,
        queued_count,
        queued_count_label,
        queued_item_label,
        next_label,
        active_queued_count_label,
        section_title: "Queue".to_string(),
        empty_title: "Nothing downloading".to_string(),
        empty_summary:
            "Launch an instance that needs a download, or install a new Minecraft version, and it will show up here."
                .to_string(),
    }
}

fn install_queue_notice_for_outcome(
    outcome: &InstallQueueEnqueueOutcome,
    spec: &InstallQueueSpec,
    placement: InstallQueuePlacement,
) -> InstallQueueNoticeViewModel {
    let label = install_queue_label(spec);
    match outcome {
        InstallQueueEnqueueOutcome::Enqueued { .. } => {
            if placement == InstallQueuePlacement::Front {
                install_queue_notice("retry_queued", "info", "Retry queued", Some(label))
            } else {
                install_queue_notice("queued", "info", "Install queued", Some(label))
            }
        }
        InstallQueueEnqueueOutcome::AlreadyActive { .. } => install_queue_notice(
            "already_active",
            "info",
            "Install already active",
            Some(label),
        ),
        InstallQueueEnqueueOutcome::AlreadyQueued { .. } => install_queue_notice(
            "already_queued",
            "info",
            "Install already queued",
            Some(label),
        ),
        InstallQueueEnqueueOutcome::MovedToFront { .. } => install_queue_notice(
            "retry_moved_next",
            "info",
            "Retry moved to the front of the queue",
            Some(label),
        ),
    }
}

fn install_queue_notice(
    state_id: &str,
    tone: &str,
    message: &str,
    detail: Option<String>,
) -> InstallQueueNoticeViewModel {
    InstallQueueNoticeViewModel {
        state_id: state_id.to_string(),
        tone: tone.to_string(),
        message: message.to_string(),
        detail,
    }
}

fn install_queue_kind(spec: &InstallQueueSpec) -> &'static str {
    match spec {
        InstallQueueSpec::Vanilla { .. } => "vanilla",
        InstallQueueSpec::Loader { .. } => "loader",
        InstallQueueSpec::Content { .. } => "content",
    }
}

fn install_queue_label(spec: &InstallQueueSpec) -> String {
    match spec {
        InstallQueueSpec::Vanilla { version_id, .. } => {
            if version_id.trim().is_empty() {
                "Minecraft".to_string()
            } else {
                format!("Minecraft {}", version_id.trim())
            }
        }
        InstallQueueSpec::Loader {
            component_id,
            minecraft_version,
            loader_version,
            ..
        } => {
            let loader_name = component_id.display_name();
            let label = if loader_version.trim().is_empty() {
                format!("{loader_name} loader")
            } else {
                format!("{loader_name} {}", loader_version.trim())
            };
            if minecraft_version.trim().is_empty() {
                label
            } else {
                format!("{label} for Minecraft {}", minecraft_version.trim())
            }
        }
        InstallQueueSpec::Content { label, .. } => label.clone(),
    }
}

fn install_queue_install_item(spec: &InstallQueueSpec) -> InstallQueueInstallItemViewModel {
    match spec {
        InstallQueueSpec::Vanilla { version_id, .. } => InstallQueueInstallItemViewModel {
            version_id: version_id.clone(),
            loader: None,
            content: None,
        },
        InstallQueueSpec::Loader {
            component_id,
            build_id,
            target_version_id,
            minecraft_version,
            loader_version,
        } => InstallQueueInstallItemViewModel {
            version_id: target_version_id.clone(),
            loader: Some(InstallQueueLoaderItemViewModel {
                component_id: component_id.as_str().to_string(),
                build_id: build_id.clone(),
                minecraft_version: minecraft_version.clone(),
                loader_version: loader_version.clone(),
            }),
            content: None,
        },
        InstallQueueSpec::Content {
            instance_id,
            action,
            ..
        } => InstallQueueInstallItemViewModel {
            version_id: instance_id.clone(),
            loader: None,
            content: Some(InstallQueueContentItemViewModel {
                instance_id: instance_id.clone(),
                action: content_action_request(action),
            }),
        },
    }
}

fn content_action_request(action: &ContentQueueAction) -> InstallQueueContentActionRequest {
    match action {
        ContentQueueAction::Install {
            selections,
            allow_incompatible,
            ..
        } => InstallQueueContentActionRequest::Install {
            selections: selections
                .iter()
                .map(|selection| InstallQueueContentSelection {
                    canonical_id: selection.canonical_id.clone(),
                    kind: selection.kind,
                    version_id: selection.version_id.clone(),
                })
                .collect(),
            allow_incompatible: *allow_incompatible,
        },
        ContentQueueAction::Uninstall { canonical_ids } => {
            InstallQueueContentActionRequest::Uninstall {
                canonical_ids: canonical_ids.clone(),
            }
        }
        ContentQueueAction::Modpack {
            canonical_id,
            version_id,
            selected_paths,
            include_overrides,
            ..
        } => InstallQueueContentActionRequest::Modpack {
            canonical_id: canonical_id.clone(),
            version_id: version_id.clone(),
            selected_paths: selected_paths.clone(),
            include_overrides: *include_overrides,
        },
    }
}

pub(crate) fn effective_install_fields(request: &InstallVersionStartRequest) -> (String, String) {
    (
        request.version_id.trim().to_string(),
        request.manifest_url.trim().to_string(),
    )
}

fn install_failure_view_model(
    progress: &InstallProgressViewModel,
    guardian: Option<&InstallGuardianOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> Option<InstallFailureViewModel> {
    if !progress.failed {
        return None;
    }

    let summary = guardian
        .map(|guardian| guardian.label.clone())
        .or_else(|| repair.map(|repair| repair.label.clone()))
        .unwrap_or_else(|| progress.label.clone());
    let mut details = Vec::new();
    push_install_failure_detail(
        &mut details,
        guardian.and_then(|guardian| guardian.detail.clone()),
    );
    if let Some(guardian) = guardian {
        for guidance in &guardian.guidance {
            push_install_failure_detail(&mut details, Some(guidance.clone()));
        }
    }
    push_install_failure_detail(&mut details, repair.map(|repair| repair.label.clone()));
    push_install_failure_detail(
        &mut details,
        repair.and_then(|repair| repair.detail.clone()),
    );

    Some(InstallFailureViewModel {
        state_id: if progress.phase_id == CONTENT_INSTANCE_REMOVED_PHASE {
            "failed_instance_removed".to_string()
        } else {
            failure_state_id(guardian, repair).to_string()
        },
        title: "Install failed".to_string(),
        tone: "err".to_string(),
        detail: details.first().cloned(),
        details,
        retry_action: install_retry_action(progress, guardian, repair),
        dismiss_action: InstallActionViewModel {
            action: "dismiss".to_string(),
            label: "Dismiss".to_string(),
            enabled: true,
            disabled_reason: None,
        },
        repair_action: install_repair_action(repair),
        summary,
    })
}

fn push_install_failure_detail(details: &mut Vec<String>, detail: Option<String>) {
    let Some(detail) = detail.map(|detail| detail.trim().to_string()) else {
        return;
    };
    if detail.is_empty() || details.iter().any(|existing| existing == &detail) {
        return;
    }
    details.push(detail);
}

fn failure_state_id(
    guardian: Option<&InstallGuardianOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> &'static str {
    if let Some(repair) = repair {
        return match repair.status.as_str() {
            "repaired" => "failed_repair_applied",
            "suppressed" => "failed_repair_suppressed",
            "blocked" => "failed_repair_blocked",
            "failed" => "failed_repair_failed",
            _ => "failed_repair_recorded",
        };
    }
    match guardian.map(|guardian| guardian.decision.as_str()) {
        Some("retry") => "failed_retryable",
        Some("block") => "failed_blocked",
        Some("suppress") => "failed_suppressed",
        Some(_) => "failed_guardian_recorded",
        None => "failed",
    }
}

fn install_retry_action(
    progress: &InstallProgressViewModel,
    guardian: Option<&InstallGuardianOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> InstallActionViewModel {
    if progress.phase_id == CONTENT_INSTANCE_REMOVED_PHASE {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: false,
            disabled_reason: Some(
                "The temporary setup instance was removed. Create the instance again to retry."
                    .to_string(),
            ),
        };
    }
    if repair.is_some_and(|repair| repair.status == "repaired") {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: true,
            disabled_reason: None,
        };
    }

    if guardian.is_some_and(|guardian| {
        guardian.decision == "block" && !blocking_guardian_allows_retry(guardian)
    }) {
        let disabled_reason = guardian_retry_disabled_reason(guardian);
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: false,
            disabled_reason: Some(disabled_reason),
        };
    }

    InstallActionViewModel {
        action: "retry".to_string(),
        label: "Retry install".to_string(),
        enabled: true,
        disabled_reason: None,
    }
}

fn blocking_guardian_allows_retry(guardian: &InstallGuardianOutcomeSummary) -> bool {
    guardian.diagnosis_id == "managed_runtime_rosetta_required"
}

fn guardian_retry_disabled_reason(guardian: Option<&InstallGuardianOutcomeSummary>) -> String {
    guardian
        .and_then(|guardian| {
            guardian
                .guidance
                .first()
                .cloned()
                .or_else(|| guardian.detail.clone())
                .or_else(|| Some(guardian.label.clone()))
        })
        .unwrap_or_else(|| "Guardian blocked immediate retry for this install.".to_string())
}

fn install_repair_action(repair: Option<&InstallGuardianRepairSummary>) -> InstallActionViewModel {
    let Some(repair) = repair else {
        return InstallActionViewModel {
            action: "repair".to_string(),
            label: "Automatic repair unavailable".to_string(),
            enabled: false,
            disabled_reason: Some("No automatic repair is available for this failure.".to_string()),
        };
    };

    let label = match repair.status.as_str() {
        "repaired" => "Automatic repair applied",
        "blocked" => "Automatic repair blocked",
        "failed" => "Automatic repair failed",
        "suppressed" => "Automatic repair paused",
        _ => "Automatic repair recorded",
    };
    InstallActionViewModel {
        action: "repair".to_string(),
        label: label.to_string(),
        enabled: false,
        disabled_reason: repair.detail.clone().or_else(|| Some(repair.label.clone())),
    }
}

fn generate_install_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos())
        .unwrap_or_default();
    format!("{prefix}-{:032x}", nanos)
}

#[cfg(test)]
mod tests;
