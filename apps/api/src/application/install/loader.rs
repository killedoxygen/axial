use super::{
    BASE_INSTALL_FAILED_MESSAGE, INSTALL_REPAIR_RESUME_MAX_DEPTH, InstallApplicationError,
    InstallProgressViewModel, InstallStartResponse, LOADER_INSTALL_INTERRUPTED_MESSAGE,
    LoaderBuildsRequest, LoaderInstallStartRequest, begin_install_operation_journal,
    emit_install_failed, generate_install_id, install_operation_id,
    record_install_failure_outcome_and_repair, record_install_operation_interrupted,
    record_install_operation_progress,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, sanitize_install_progress,
    stage_install_version_command,
};
use crate::application::{InstallVersionCommand, instances::invalidate_create_view_source};
use crate::dto::loaders::{
    LoaderBuildsResponse, LoaderComponentsResponse, LoaderGameVersionsResponse,
};
use crate::guardian::GuardianArtifactRepairStatus;
use crate::install_runtime::prewarm_version_runtime;
use crate::state::{AppState, InstallStore};
use axum::{Json, http::StatusCode};
use croopor_minecraft::{
    DownloadProgress, LoaderComponentId, LoaderError, LoaderProviderFailureKind, fetch_builds,
    fetch_components, fetch_supported_versions, install_build, resolve_build_record,
};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

const LOADER_INSTALL_SCOPE: &str = "loader";
const VANILLA_INSTALL_SCOPE: &str = "vanilla";

pub async fn start_loader_install(
    state: &AppState,
    request: LoaderInstallStartRequest,
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
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let library_dir_path = PathBuf::from(&library_dir);

    let build = resolve_build_record(library_dir_path.as_path(), request.component_id, &build_id)
        .await
        .map_err(loader_error_response)?;

    let (install_version_key, install_manifest_key) =
        loader_install_key_fields(build.component_id, &build.build_id, &build.version_id);
    let target_version_id = build.version_id.clone();
    let install_id = generate_install_id("loader-install");
    let (install_id, inserted) = state
        .installs()
        .insert_or_existing_active_scoped(
            LOADER_INSTALL_SCOPE.to_string(),
            install_id,
            install_version_key,
            install_manifest_key,
        )
        .await;
    let operation_id = install_operation_id(&install_id);
    let staging = stage_install_version_command(
        InstallVersionCommand {
            version_id: target_version_id.clone(),
            manifest_url: None,
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
    begin_install_operation_journal(state.journals(), &operation_id, &target_version_id);

    let store = state.installs().clone();
    let journals = state.journals().clone();
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
    InstallStore::spawn_tracked_worker_with_interrupt_handler(
        store,
        install_id_task,
        interrupted_loader_install_progress(),
        async move {
            let (progress_tx, mut progress_rx) = mpsc::unbounded_channel::<DownloadProgress>();
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
                );
                emit_install_failed(
                    worker_telemetry.as_ref(),
                    progress
                        .error
                        .as_deref()
                        .unwrap_or(BASE_INSTALL_FAILED_MESSAGE),
                );
                let _ = progress_tx.send(progress);
                drop(progress_tx);
                let _ = store_task.await;
                return;
            }

            let final_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let mut repair_resume_depth = 0_u8;
            loop {
                if let Ok(mut final_progress) = final_progress.lock() {
                    *final_progress = None;
                }
                let final_progress_for_install = Arc::clone(&final_progress);
                let result = install_build(&library_dir, build.clone(), |progress| {
                    if progress.done {
                        if let Ok(mut final_progress) = final_progress_for_install.lock() {
                            *final_progress = Some(progress);
                        }
                        return;
                    }
                    let _ = progress_tx.send(progress);
                })
                .await;

                if let Err(error) = result {
                    let observed_at = chrono::Utc::now().to_rfc3339();
                    let repair_outcome = match &error {
                        LoaderError::BaseInstallFailed { facts, descriptors } => {
                            if facts.is_empty() {
                                record_loader_base_install_dependency_guardian_failure_outcome(
                                    worker_journals.as_ref(),
                                    &worker_operation_id,
                                    &loader_target_id,
                                    &base_version_id,
                                );
                                None
                            } else {
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
                            );
                            None
                        }
                    };
                    if repair_resume_depth < INSTALL_REPAIR_RESUME_MAX_DEPTH
                        && repair_outcome.as_ref().is_some_and(|outcome| {
                            outcome.status == GuardianArtifactRepairStatus::Repaired
                        })
                    {
                        repair_resume_depth += 1;
                        continue;
                    }
                    let progress = loader_error_progress(&error);
                    emit_install_failed(
                        worker_telemetry.as_ref(),
                        progress
                            .error
                            .as_deref()
                            .unwrap_or(BASE_INSTALL_FAILED_MESSAGE),
                    );
                    let _ = progress_tx.send(progress);
                    drop(progress_tx);
                    let _ = store_task.await;
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
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
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

    let Some((history, mut receiver, done)) = store.subscribe(&install_id).await else {
        return Ok(());
    };

    for progress in history {
        if progress.done {
            return if progress.error.is_some() {
                Err(base_install_failed_progress())
            } else {
                Ok(())
            };
        }
        let _ = progress_tx.send(progress);
    }
    if done {
        return Ok(());
    }

    loop {
        match receiver.recv().await {
            Ok(progress) => {
                if progress.done {
                    return if progress.error.is_some() {
                        Err(base_install_failed_progress())
                    } else {
                        Ok(())
                    };
                }
                let _ = progress_tx.send(progress);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
        }
    }
}

pub fn loader_error_response(error: LoaderError) -> InstallApplicationError {
    let status = match error {
        LoaderError::InvalidMinecraftVersion
        | LoaderError::InvalidBuildId
        | LoaderError::InvalidComponentId => StatusCode::BAD_REQUEST,
        LoaderError::BuildNotFound(_) => StatusCode::NOT_FOUND,
        LoaderError::CatalogStale => StatusCode::PRECONDITION_FAILED,
        LoaderError::MissingLibraryDir => StatusCode::PRECONDITION_FAILED,
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
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(public_loader_error_message(error).to_string()),
        done: true,
    }
}

pub(crate) fn base_install_failed_progress() -> DownloadProgress {
    DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(BASE_INSTALL_FAILED_MESSAGE.to_string()),
        done: true,
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
    }
}

fn public_loader_error_message(error: &LoaderError) -> &'static str {
    match error {
        LoaderError::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderError::InvalidBuildId => "Invalid loader build.",
        LoaderError::InvalidComponentId => "Invalid loader component.",
        LoaderError::MissingLibraryDir => "Croopor library is not configured",
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
            "Loader provider returned data Croopor could not trust. Try again later."
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
        LoaderError::Request(_) => {
            "Loader service request failed. Check your connection and try again."
        }
        LoaderError::Download(_) => "Loader download failed. Check your connection and try again.",
        LoaderError::Parse(_) => "Loader service returned unreadable data. Try again later.",
        LoaderError::Io(_) => {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderError::Other(_) => "Loader operation failed. Try again.",
    }
}
