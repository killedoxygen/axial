use super::repair::InstallRepairResume;
use super::{
    BASE_INSTALL_FAILED_MESSAGE, InstallApplicationError, InstallProgressCoalescer,
    InstallProgressViewModel, InstallStartResponse, LOADER_INSTALL_INTERRUPTED_MESSAGE,
    LoaderBuildsRequest, LoaderInstallStartRequest,
    begin_install_journal_with_owned_reconciliation, emit_install_failed,
    finish_install_progress_task, generate_install_id, install_journal_error_response,
    install_operation_id, operation::install_progress_with_terminal_error,
    record_and_emit_install_progress, record_install_failure_outcome_and_repair,
    record_install_failure_outcome_and_repair_for_error, record_install_operation_interrupted,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, sanitize_install_progress,
    stage_install_version_command,
};
use crate::application::{InstallVersionCommand, instances::invalidate_create_view_source};
use crate::dto::loaders::{
    LoaderBuildsResponse, LoaderComponentsResponse, LoaderGameVersionsResponse,
};
use crate::install_runtime::prewarm_version_runtime;
use crate::state::InstallInitializationStatus;
use crate::state::{AppState, InstallStore, ProducerLease};
use axial_minecraft::{
    DownloadProgress, LoaderComponentId, LoaderError, LoaderProviderFailureKind, fetch_builds,
    fetch_components, fetch_supported_versions, install_build, resolve_build_record,
};
use axum::{Json, http::StatusCode};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const LOADER_INSTALL_SCOPE: &str = "loader";
const VANILLA_INSTALL_SCOPE: &str = "vanilla";

pub(super) async fn start_loader_install_owned(
    state: &AppState,
    request: LoaderInstallStartRequest,
    producer: &ProducerLease,
) -> Result<InstallStartResponse, InstallApplicationError> {
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
    let library_dir_path = PathBuf::from(&library_dir);

    let build = resolve_build_record(library_dir_path.as_path(), request.component_id, &build_id)
        .await
        .map_err(loader_error_response)?;

    let (install_version_key, install_manifest_key) =
        loader_install_key_fields(build.component_id, &build.build_id, &build.version_id);
    let target_version_id = build.version_id.clone();
    let install_id = loop {
        let candidate = generate_install_id("loader-install");
        let (install_id, inserted) = state
            .installs()
            .insert_or_existing_active_scoped(
                LOADER_INSTALL_SCOPE.to_string(),
                candidate,
                install_version_key.clone(),
                install_manifest_key.clone(),
            )
            .await;
        if inserted {
            break install_id;
        }
        match state.installs().wait_for_initialization(&install_id).await {
            InstallInitializationStatus::Initialized => {
                return Ok(InstallStartResponse {
                    operation_id: install_operation_id(&install_id),
                    install_id,
                    view_model: InstallProgressViewModel::starting(),
                });
            }
            InstallInitializationStatus::Reconciling => {
                return Err(install_journal_error_response());
            }
            InstallInitializationStatus::Removed => {}
        }
    };
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: target_version_id.clone(),
            manifest_url: None,
        },
        install_id.clone(),
        operation_id.clone(),
    );
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let reservation = begin_install_journal_with_owned_reconciliation(
        store.clone(),
        journals.clone(),
        install_id.clone(),
        operation_id.clone(),
        target_version_id.clone(),
        producer,
    )
    .await
    .map_err(|_| install_journal_error_response())?;
    if !store.mark_initialized(&install_id).await {
        return Err(install_journal_error_response());
    }

    let telemetry = state.telemetry().clone();
    let library_dir = PathBuf::from(library_dir);
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id_task.clone();
    let worker_failure_memory = state.failure_memory().clone();
    let worker_telemetry = telemetry.clone();
    let progress_owner = producer.claim_child();
    InstallStore::spawn_tracked_worker_with_interrupt_handler_owned(
        store,
        producer.claim_child(),
        install_id_task,
        interrupted_loader_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
            let journal_failed = Arc::new(tokio::sync::Notify::new());
            let store_task = {
                let store = worker_store.clone();
                let install_id = worker_install_id.clone();
                let journals = worker_journals.clone();
                let operation_id = worker_operation_id.clone();
                let journal_failed = journal_failed.clone();
                progress_owner.spawn_joinable(async move {
                    let mut coalescer = InstallProgressCoalescer::default();
                    let mut last_journal_phase = None;
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        for progress in coalescer.push(progress) {
                            if !record_and_emit_install_progress(
                                store.as_ref(),
                                journals.as_ref(),
                                &operation_id,
                                &install_id,
                                progress,
                                &mut last_journal_phase,
                            )
                            .await
                            {
                                journal_failed.notify_one();
                                return false;
                            }
                        }
                    }
                    if let Some(progress) = coalescer.flush()
                        && !record_and_emit_install_progress(
                            store.as_ref(),
                            journals.as_ref(),
                            &operation_id,
                            &install_id,
                            progress,
                            &mut last_journal_phase,
                        )
                        .await
                    {
                        journal_failed.notify_one();
                        return false;
                    }
                    true
                })
            };

            let version_id = build.version_id.clone();
            let base_version_id = build.minecraft_version.clone();
            let loader_target_id = format!(
                "loader_{}_{}",
                build.component_id.short_key(),
                build.build_id
            );
            if let Err(progress) =
                wait_for_active_vanilla_base_install(&worker_store, &base_version_id, &progress_tx)
                    .await
            {
                record_loader_base_install_dependency_guardian_failure_outcome(
                    worker_journals.as_ref(),
                    &worker_operation_id,
                    &loader_target_id,
                    &base_version_id,
                )
                .await
                .ok();
                let failure_summary = progress
                    .error
                    .clone()
                    .unwrap_or_else(|| BASE_INSTALL_FAILED_MESSAGE.to_string());
                let _ = progress_tx.send(progress);
                drop(progress_tx);
                if finish_install_progress_task(
                    store_task,
                    worker_store.as_ref(),
                    &worker_install_id,
                )
                .await
                {
                    emit_install_failed(worker_telemetry.as_ref(), &failure_summary);
                }
                return;
            }

            let final_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let mut repair_resume = InstallRepairResume::default();
            loop {
                if let Ok(mut final_progress) = final_progress.lock() {
                    *final_progress = None;
                }
                let final_progress_for_install = Arc::clone(&final_progress);
                let result = {
                    let install = install_build(&library_dir, build.clone(), |progress| {
                        if progress.done {
                            if let Ok(mut final_progress) = final_progress_for_install.lock() {
                                *final_progress = Some(progress);
                            }
                            return;
                        }
                        let _ = progress_tx.send(progress);
                    });
                    tokio::pin!(install);
                    tokio::select! {
                        result = &mut install => Some(result),
                        () = journal_failed.notified() => None,
                    }
                };
                let Some(result) = result else {
                    drop(progress_tx);
                    let _ = finish_install_progress_task(
                        store_task,
                        worker_store.as_ref(),
                        &worker_install_id,
                    )
                    .await;
                    return;
                };

                if let Err(error) = result {
                    let observed_at = chrono::Utc::now().to_rfc3339();
                    let repair_outcome = match &error {
                        LoaderError::BaseInstallFailed {
                            error: base_error,
                            facts,
                            descriptors,
                        } => {
                            // fallback outcome, recorded first so the
                            // error-aware one below wins on journal read
                            if facts.is_empty() {
                                record_loader_base_install_dependency_guardian_failure_outcome(
                                    worker_journals.as_ref(),
                                    &worker_operation_id,
                                    &loader_target_id,
                                    &base_version_id,
                                )
                                .await
                                .ok();
                            }
                            record_install_failure_outcome_and_repair_for_error(
                                worker_journals.as_ref(),
                                worker_failure_memory.as_ref(),
                                &worker_operation_id,
                                base_error,
                                facts,
                                descriptors,
                                &observed_at,
                            )
                            .await
                        }
                        LoaderError::ArtifactDownloadFailed { facts, descriptors } => {
                            record_install_failure_outcome_and_repair(
                                worker_journals.as_ref(),
                                worker_failure_memory.as_ref(),
                                &worker_operation_id,
                                facts,
                                descriptors,
                                &observed_at,
                            )
                            .await
                        }
                        _ => {
                            record_loader_install_operation_guardian_failure_outcome(
                                worker_journals.as_ref(),
                                worker_failure_memory.as_ref(),
                                &worker_operation_id,
                                &loader_target_id,
                                &error,
                                &observed_at,
                            )
                            .await
                            .ok();
                            None
                        }
                    };
                    if repair_resume.resume_after(repair_outcome.as_ref()) {
                        continue;
                    }
                    let progress = loader_error_progress(&error);
                    let failure_summary = progress
                        .error
                        .clone()
                        .unwrap_or_else(|| BASE_INSTALL_FAILED_MESSAGE.to_string());
                    let _ = progress_tx.send(progress);
                    drop(progress_tx);
                    if finish_install_progress_task(
                        store_task,
                        worker_store.as_ref(),
                        &worker_install_id,
                    )
                    .await
                    {
                        emit_install_failed(worker_telemetry.as_ref(), &failure_summary);
                    }
                    return;
                }
                break;
            }

            let _ = prewarm_version_runtime(&library_dir, &version_id, |progress| {
                let _ = progress_tx.send(progress);
            })
            .await;
            if let Some(progress) = final_progress
                .lock()
                .ok()
                .and_then(|mut progress| progress.take())
            {
                let _ = progress_tx.send(progress);
            } else {
                let _ = progress_tx.send(loader_install_done_progress());
            }

            drop(progress_tx);
            finish_install_progress_task(store_task, worker_store.as_ref(), &worker_install_id)
                .await;
        },
        move |progress| async move {
            if record_install_operation_interrupted(
                journals.as_ref(),
                &operation_id_task,
                &progress,
            )
            .await
            .is_err()
            {
                tracing::warn!("failed to commit interrupted loader-install journal");
                return false;
            }
            true
        },
    );
    reservation.hand_off();

    Ok(InstallStartResponse {
        install_id,
        operation_id: staging.result.operation_id.unwrap_or(operation_id),
        view_model: InstallProgressViewModel::starting(),
    })
}

pub fn loader_components() -> LoaderComponentsResponse {
    LoaderComponentsResponse {
        components: fetch_components(),
    }
}

pub async fn loader_builds(
    state: &AppState,
    request: LoaderBuildsRequest,
) -> Result<LoaderBuildsResponse, InstallApplicationError> {
    if request.mc_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "mc_version query parameter is required" })),
        ));
    }
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )
    })?;

    let library_dir = PathBuf::from(library_dir);
    let (builds, catalog) = fetch_builds(
        library_dir.as_path(),
        request.component_id,
        &request.mc_version,
    )
    .await
    .map_err(loader_error_response)?;
    invalidate_create_view_source(library_dir.as_path(), request.component_id.as_str());
    Ok(LoaderBuildsResponse { builds, catalog })
}

pub async fn loader_game_versions(
    state: &AppState,
    component_id: LoaderComponentId,
) -> Result<LoaderGameVersionsResponse, InstallApplicationError> {
    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Axial library is not configured" })),
        )
    })?;

    fetch_supported_versions(PathBuf::from(library_dir).as_path(), component_id)
        .await
        .map(|(versions, catalog)| LoaderGameVersionsResponse { versions, catalog })
        .map_err(loader_error_response)
}

pub(crate) fn loader_install_key_fields(
    component_id: LoaderComponentId,
    build_id: &str,
    version_id: &str,
) -> (String, String) {
    (
        format!("loader:{}:{}", component_id.as_str(), version_id.trim()),
        format!("loader:{}:{}", component_id.as_str(), build_id.trim()),
    )
}

pub(crate) async fn wait_for_active_vanilla_base_install(
    store: &InstallStore,
    version_id: &str,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
) -> Result<(), DownloadProgress> {
    let Some(install_id) = store
        .active_install_for_scope_and_version(VANILLA_INSTALL_SCOPE, version_id)
        .await
    else {
        return Ok(());
    };

    let Some((snapshot, mut receiver)) = store.subscribe_records(&install_id).await else {
        return Ok(());
    };

    if let Some(record) = snapshot.latest {
        if record.progress.done {
            return if record.progress.error.is_some() {
                Err(base_install_failed_progress())
            } else {
                Ok(())
            };
        }
        let _ = progress_tx.send(record.progress);
    }
    if snapshot.done {
        return Ok(());
    }

    loop {
        match receiver.recv().await {
            Ok(record) => {
                if record.progress.done {
                    return if record.progress.error.is_some() {
                        Err(base_install_failed_progress())
                    } else {
                        Ok(())
                    };
                }
                let _ = progress_tx.send(record.progress);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

pub fn loader_error_response(error: LoaderError) -> InstallApplicationError {
    let status = match error {
        LoaderError::InvalidMinecraftVersion | LoaderError::InvalidBuildId => {
            StatusCode::BAD_REQUEST
        }
        LoaderError::BuildNotFound(_) => StatusCode::NOT_FOUND,
        LoaderError::CatalogStale => StatusCode::PRECONDITION_FAILED,
        LoaderError::CatalogUnavailable {
            provider_failure_kind: Some(LoaderProviderFailureKind::HttpNotFound),
            ..
        }
        | LoaderError::ProviderUnavailable {
            kind: LoaderProviderFailureKind::HttpNotFound,
            ..
        }
        | LoaderError::ProviderDataInvalid {
            kind: LoaderProviderFailureKind::HttpNotFound,
            ..
        } => StatusCode::NOT_FOUND,
        LoaderError::CatalogUnavailable { .. }
        | LoaderError::ArtifactMissing(_)
        | LoaderError::BaseInstallFailed { .. }
        | LoaderError::ArtifactDownloadFailed { .. }
        | LoaderError::ProviderUnavailable { .. }
        | LoaderError::ProviderDataInvalid { .. } => StatusCode::BAD_GATEWAY,
        _ => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(serde_json::json!({
            "error": public_loader_error_message(&error),
            "failure_kind": error.failure_kind(),
        })),
    )
}

pub(crate) fn loader_error_progress(error: &LoaderError) -> DownloadProgress {
    let progress = DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(public_loader_error_message(error).to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };
    if let LoaderError::BaseInstallFailed {
        error: base_error, ..
    } = error
    {
        return install_progress_with_terminal_error(progress, base_error);
    }
    progress
}

pub(crate) fn base_install_failed_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(BASE_INSTALL_FAILED_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

pub(crate) fn loader_install_done_progress() -> DownloadProgress {
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

pub(crate) fn interrupted_loader_install_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(LOADER_INSTALL_INTERRUPTED_MESSAGE.to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    }
}

fn public_loader_error_message(error: &LoaderError) -> &'static str {
    match error {
        LoaderError::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderError::InvalidBuildId => "Invalid loader build.",
        LoaderError::CatalogUnavailable { .. } => {
            "Loader catalog is unavailable. Check your connection and try again."
        }
        LoaderError::CatalogStale => {
            "Loader catalog needs a fresh provider check before this build can be installed."
        }
        LoaderError::BuildNotFound(_) => "Selected loader build is not available.",
        LoaderError::ArtifactMissing(_) => {
            "Loader artifact is unavailable. Try another build or component."
        }
        LoaderError::ProviderUnavailable { .. } => {
            "Loader provider is unavailable. Check your connection and try again."
        }
        LoaderError::ProviderDataInvalid { .. } => {
            "Loader provider returned data Axial could not trust. Try again later."
        }
        LoaderError::InvalidProfile(_) => "Loader profile is invalid. Try another build.",
        LoaderError::Verify(_) => {
            "Loader install verification failed. Try again or choose another build."
        }
        LoaderError::BaseInstallFailed { .. } => {
            "Base game install failed. Retry the install from Downloads."
        }
        LoaderError::ArtifactDownloadFailed { .. } => {
            "Loader download failed. Check your connection and try again."
        }
        LoaderError::Parse(_) => "Loader install data could not be read. Try again.",
        LoaderError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied => {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderError::Io(_) => "Loader file operation failed. Restart Axial and try again.",
        LoaderError::ProcessorFailed(_) => {
            "Loader installer processor failed. Retry or choose another build."
        }
        LoaderError::InstallExecutionFailed(_) => {
            "Loader installer could not complete. Restart Axial and try again."
        }
    }
}
