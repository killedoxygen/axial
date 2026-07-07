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
use crate::application::instances::invalidate_create_view_installed_scan;
use crate::guardian::{GuardianArtifactRepairOutcome, GuardianArtifactRepairStatus};
use crate::observability::{
    operation_journal_proof_record,
    telemetry::{
        TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
    },
};
use crate::state::AppState;
use crate::state::{
    ActiveQueuedInstallEntry, InstallQueueEnqueueOutcome, InstallQueuePlacement,
    InstallQueueSnapshot, InstallQueueSpec, InstallStore, QueuedInstallEntry,
};
use axum::{Json, http::StatusCode};
use croopor_minecraft::{
    DownloadProgress, Downloader, LoaderComponentId,
    download::{ExecutionDownloadFact, SelectedDownloadArtifactDescriptor},
    resolve_build_record,
};
use std::path::PathBuf;
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
    InstallQueueActiveViewModel, InstallQueueInstallItemViewModel, InstallQueueLoaderItemViewModel,
    InstallQueueNoticeViewModel, InstallQueueRequest, InstallQueueStateResponse,
    InstallQueueViewModel, InstallQueuedItemViewModel, InstallStartResponse, InstallStatusResponse,
    InstallVersionStaging, InstallVersionStartRequest, LoaderBuildsRequest,
    LoaderInstallStartRequest,
};
pub use operation::{
    begin_install_operation_journal, install_guardian_outcome_summary_from_journal,
    install_operation_id, loader_install_progress_view_model, public_loader_install_progress_json,
    public_vanilla_install_progress_json, record_install_operation_guardian_evidence,
    record_install_operation_guardian_failure_outcome,
    record_install_operation_guardian_failure_outcome_with_memory,
    record_install_operation_interrupted, record_install_operation_progress,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, sanitize_install_progress,
    stage_install_version_command, vanilla_install_progress_view_model,
};
use operation::{
    install_failure_point_from_journal, install_journal_is_terminal,
    install_progress_history_from_journal, interrupted_install_progress,
    observed_install_failure_progress, public_install_id,
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
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        record_install_operation_progress(
                            journals.as_ref(),
                            &operation_id,
                            &progress,
                            &mut last_journal_phase,
                        );
                        store.emit(&install_id, progress).await;
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
                if install_result.is_ok() {
                    break (true, attempt_terminal_progress);
                }
                let observed_at = chrono::Utc::now().to_rfc3339();
                let repair_outcome = record_install_failure_outcome_and_repair(
                    worker_journals.as_ref(),
                    worker_failure_memory.as_ref(),
                    &worker_operation_id,
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
                    Some(terminal_failure_progress_or_default(
                        attempt_terminal_progress,
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

fn vanilla_install_done_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "done".to_string(),
        current: 1,
        total: 1,
        file: None,
        error: None,
        done: true,
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
        .map(|snapshot| {
            if snapshot.history.is_empty() {
                journal
                    .as_ref()
                    .map(install_progress_history_from_journal)
                    .unwrap_or_default()
            } else {
                snapshot.history
            }
        })
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
    Ok(install_queue_state_response(state, None, started_install).await)
}

pub async fn enqueue_install(
    state: &AppState,
    request: InstallQueueRequest,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(state, request, InstallQueuePlacement::Back).await
}

pub async fn retry_install(
    state: &AppState,
    request: InstallQueueRequest,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    enqueue_install_with_placement(state, request, InstallQueuePlacement::Front).await
}

pub async fn remove_queued_install(
    state: &AppState,
    queue_id: &str,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let removed = state.installs().remove_queued_install(queue_id).await;
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
    Ok(install_queue_state_response(state, notice, None).await)
}

async fn enqueue_install_with_placement(
    state: &AppState,
    request: InstallQueueRequest,
    placement: InstallQueuePlacement,
) -> Result<InstallQueueStateResponse, InstallApplicationError> {
    let spec = install_queue_spec_from_request(state, request).await?;
    let queue_id = generate_install_id("install-queue");
    let outcome = state
        .installs()
        .enqueue_queued_install(queue_id, spec.clone(), placement)
        .await;
    let notice = Some(install_queue_notice_for_outcome(&outcome, &spec, placement));
    let started_install = maybe_start_next_queued_install(state).await?;
    Ok(install_queue_state_response(state, notice, started_install).await)
}

async fn install_queue_spec_from_request(
    state: &AppState,
    request: InstallQueueRequest,
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
                    Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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
                    Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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
        _ => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "install kind is required" })),
        )),
    }
}

async fn maybe_start_next_queued_install(
    state: &AppState,
) -> Result<Option<InstallStartResponse>, InstallApplicationError> {
    let Some(entry) = state.installs().reserve_next_queued_install().await else {
        return Ok(None);
    };
    let started = match start_queued_install(state, &entry.spec).await {
        Ok(started) => started,
        Err(error) => {
            state
                .installs()
                .discard_active_queued_install(&entry.queue_id)
                .await;
            return Err(error);
        }
    };
    state
        .installs()
        .mark_queued_install_started(&entry.queue_id, started.install_id.clone())
        .await;
    spawn_install_queue_monitor(state.clone(), started.install_id.clone());
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
    }
}

fn spawn_install_queue_monitor(state: AppState, install_id: String) {
    tokio::spawn(async move {
        wait_for_install_terminal(&state, &install_id).await;
        invalidate_create_view_installed_scan();
        state
            .installs()
            .clear_active_queued_install(&install_id)
            .await;
        let _ = maybe_start_next_queued_install(&state).await;
    });
}

async fn wait_for_install_terminal(state: &AppState, install_id: &str) {
    let Some((history, mut receiver, done)) = state.installs().subscribe(install_id).await else {
        return;
    };
    if done || history.iter().any(|progress| progress.done) {
        return;
    }
    loop {
        match receiver.recv().await {
            Ok(progress) if progress.done => return,
            Ok(_) => {}
            Err(RecvError::Lagged(_)) => {}
            Err(RecvError::Closed) => return,
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
    let progress = snapshot.and_then(|snapshot| snapshot.history.last().cloned());
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
    let active_queued_count_label = (queued_count > 0).then(|| format!(" · {queued_count_label}"));
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
    }
}

fn install_queue_install_item(spec: &InstallQueueSpec) -> InstallQueueInstallItemViewModel {
    match spec {
        InstallQueueSpec::Vanilla { version_id, .. } => InstallQueueInstallItemViewModel {
            version_id: version_id.clone(),
            loader: None,
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
        state_id: failure_state_id(guardian, repair).to_string(),
        title: "Install failed".to_string(),
        tone: "err".to_string(),
        detail: details.first().cloned(),
        details,
        retry_action: install_retry_action(guardian, repair),
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
    guardian: Option<&InstallGuardianOutcomeSummary>,
    repair: Option<&InstallGuardianRepairSummary>,
) -> InstallActionViewModel {
    if repair.is_some_and(|repair| repair.status == "repaired") {
        return InstallActionViewModel {
            action: "retry".to_string(),
            label: "Retry install".to_string(),
            enabled: true,
            disabled_reason: None,
        };
    }

    if guardian.is_some_and(|guardian| guardian.decision == "block") {
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
