use super::{
    BASE_INSTALL_FAILED_MESSAGE, INSTALL_FAILURE_MESSAGE, InstallApplicationError,
    InstallForegroundActivity, InstallProgressCoalescer, InstallProgressJournalTracker,
    InstallProgressPresenter, InstallProgressViewModel, InstallStartResponse,
    LOADER_INSTALL_INTERRUPTED_MESSAGE, LoaderBuildsRequest, LoaderInstallStartRequest,
    await_managed_install_settlement_retaining, begin_install_journal_with_owned_reconciliation,
    emit_install_failed, finish_install_progress_task, generate_install_id,
    install_journal_error_response, known_good_acceptance_download_error,
    mint_available_install_operation_id,
    operation::install_progress_with_terminal_error, record_and_emit_install_progress,
    record_install_failure_outcome, record_install_failure_outcome_for_error,
    record_install_operation_interrupted,
    record_loader_base_install_dependency_guardian_failure_outcome,
    record_loader_install_operation_guardian_failure_outcome, register_install_foreground,
    retain_install_foreground, sanitize_install_progress, spawn_install_foreground_retention,
    terminal_failure_progress_or_default,
};
use crate::application::instances::invalidate_create_view_source;
use crate::dto::loaders::{
    LoaderBuildsResponse, LoaderComponentsResponse, LoaderGameVersionsResponse,
};
use crate::state::{
    AppState, InstallAdmissionError, InstallInitializationStatus, InstallProgressRecord,
    InstallSnapshot, InstallStore, IntegrityForegroundLease, ProducerLease,
};
use axial_minecraft::loaders::LoaderActiveInstallFailure;
use axial_minecraft::{
    DownloadProgress, LoaderComponentId, LoaderError, LoaderInstallError, LoaderInstallFailureKind,
    LoaderPreOperationFailureKind, LoaderProviderFailureKind, fetch_builds, fetch_components,
    fetch_supported_versions, install_build, resolve_build_record_for_install,
};
use axum::{Json, http::StatusCode};
use std::future::Future;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

pub(super) async fn start_loader_install_with_foreground(
    state: &AppState,
    request: LoaderInstallStartRequest,
    producer: &ProducerLease,
    inherited_foreground: Option<IntegrityForegroundLease>,
) -> Result<InstallStartResponse, InstallApplicationError> {
    let update_admission = state
        .try_admit_update_sensitive_operation()
        .map_err(super::install_update_admission_error_response)?;
    let build_id = request.build_id.trim().to_string();
    if build_id.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "build_id is required" })),
        ));
    }
    super::require_available_install_library(state)?;

    let foreground = match inherited_foreground {
        Some(foreground) => foreground,
        None => {
            register_install_foreground(state)?
                .wait_for_settlement()
                .await
        }
    };
    let build = resolve_build_record_for_install(request.component_id, &build_id)
        .await
        .map_err(loader_pre_operation_error_response)?;

    let target_version_id = build.version_id.clone();
    let mut admitted_install = None;
    for _ in 0..super::OPERATION_ID_RESERVATION_ATTEMPTS {
        let candidate = generate_install_id("loader-install");
        if super::operation::install_operation_journal_for_session(state.journals(), &candidate)
            .is_some()
        {
            continue;
        }
        let Some(candidate_operation_id) = mint_available_install_operation_id(state).await else {
            break;
        };
        let (install_id, inserted) = match state
            .installs()
            .admit_or_existing_loader(
                candidate,
                candidate_operation_id.clone(),
                build.component_id,
                build.build_id.clone(),
            )
            .await
        {
            Ok(admission) => admission,
            Err(
                InstallAdmissionError::InstallIdCollision
                | InstallAdmissionError::OperationIdCollision,
            ) => continue,
        };
        if inserted {
            admitted_install = Some((install_id, candidate_operation_id));
            break;
        }
        match state.installs().wait_for_initialization(&install_id).await {
            InstallInitializationStatus::Initialized => {
                let Some(operation_id) = state.installs().operation_id(&install_id).await else {
                    return Err(install_journal_error_response());
                };
                return Ok(InstallStartResponse {
                    operation_id,
                    install_id,
                    view_model: InstallProgressViewModel::starting(),
                });
            }
            InstallInitializationStatus::Reconciling => {
                return Err(install_journal_error_response());
            }
            InstallInitializationStatus::Removed => {}
        }
    }
    let Some((install_id, operation_id)) = admitted_install else {
        return Err(install_journal_error_response());
    };
    let store = state.installs().clone();
    let journals = state.journals().clone();
    let reservation = begin_install_journal_with_owned_reconciliation(
        store.clone(),
        journals.clone(),
        install_id.clone(),
        operation_id.clone(),
        target_version_id.clone(),
        producer,
        foreground,
    )
    .await
    .map_err(|_| install_journal_error_response())?;
    if !store.mark_initialized(&install_id).await {
        return Err(install_journal_error_response());
    }

    let telemetry = state.telemetry().clone();
    let install_id_task = install_id.clone();
    let operation_id_task = operation_id.clone();

    let worker_store = store.clone();
    let worker_install_id = install_id_task.clone();
    let worker_journals = journals.clone();
    let worker_operation_id = operation_id_task.clone();
    let worker_failure_memory = state.failure_memory().clone();
    let worker_telemetry = telemetry.clone();
    let worker_state = state.clone();
    let worker_runtime_cache = state.managed_runtime_cache().clone();
    let progress_owner = producer.claim_child();
    let guardian_owner = producer.claim_child();
    let foreground = InstallForegroundActivity::new_with_update_admission(
        reservation.hand_off(),
        update_admission,
    );
    let worker_foreground = foreground.clone();
    let interrupted_foreground = foreground.clone();
    let interrupted_state = state.clone();
    spawn_install_foreground_retention(
        state.clone(),
        install_id_task.clone(),
        producer.claim_child(),
        foreground,
    );
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
                    let mut presenter = InstallProgressPresenter::default();
                    let mut progress_journal = InstallProgressJournalTracker::default();
                    while let Some(progress) = progress_rx.recv().await {
                        let progress = sanitize_install_progress(progress);
                        for progress in coalescer.push(progress) {
                            if !record_and_emit_install_progress(
                                store.as_ref(),
                                journals.as_ref(),
                                &operation_id,
                                &install_id,
                                progress,
                                &mut progress_journal,
                                &mut presenter,
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
                            &mut progress_journal,
                            &mut presenter,
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
            let observed_base =
                observe_active_vanilla_base_install(&worker_store, &base_version_id).await;
            let (loader_foreground, base_install) = match observed_base {
                Ok(Some(observed)) => {
                    worker_foreground.release();
                    let base_install =
                        wait_for_observed_vanilla_base_install(observed, &progress_tx).await;
                    let Some(foreground) =
                        retain_install_foreground(&worker_state, &worker_foreground).await
                    else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return;
                    };
                    (foreground, base_install)
                }
                Ok(None) => {
                    let Some(foreground) = worker_foreground.retained() else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return;
                    };
                    (foreground, Ok(()))
                }
                Err(progress) => {
                    let Some(foreground) = worker_foreground.retained() else {
                        drop(progress_tx);
                        let _ = finish_install_progress_task(store_task).await;
                        return;
                    };
                    (foreground, Err(progress))
                }
            };
            if let Err(progress) = base_install {
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
                if finish_install_progress_task(store_task).await {
                    emit_install_failed(worker_telemetry.as_ref(), &failure_summary);
                }
                return;
            }

            let final_progress = Arc::new(Mutex::new(None::<DownloadProgress>));
            let final_progress_for_install = Arc::clone(&final_progress);
            let settlement = match worker_state.admit_managed_artifact_mutation() {
                Ok(mutation) => match worker_state.try_acquire_managed_library() {
                    Ok(library_operation) => {
                        let install = install_build(
                            library_operation.core(),
                            worker_runtime_cache.clone(),
                            build.clone(),
                            |progress| {
                                if progress.done {
                                    if let Ok(mut final_progress) =
                                        final_progress_for_install.lock()
                                    {
                                        *final_progress = Some(progress);
                                    }
                                    return;
                                }
                                let _ = progress_tx.send(progress);
                            },
                        );
                        await_managed_install_settlement_retaining(
                            mutation,
                            install,
                            journal_failed.notified(),
                        )
                        .await
                        .map(|(result, mutation)| {
                            (result, Some((mutation, library_operation)))
                        })
                    }
                    Err(error) => {
                        drop(mutation);
                        Some((
                            Err(LoaderInstallError::from(LoaderError::Io(error))),
                            None,
                        ))
                    }
                },
                Err(error) => Some((
                    Err(LoaderInstallError::from(LoaderError::Io(
                        std::io::Error::other(error),
                    ))),
                    None,
                )),
            };
            let Some((result, authority)) = settlement else {
                drop(progress_tx);
                let _ = finish_install_progress_task(store_task).await;
                return;
            };

            match result {
                Err(error) => {
                    let observed_at = chrono::Utc::now().to_rfc3339();
                    let progress = loader_install_error_progress(&error);
                    dispatch_loader_install_failure(
                        &guardian_owner,
                        worker_journals.clone(),
                        worker_failure_memory.clone(),
                        LoaderInstallFailureRequest {
                            operation_id: &worker_operation_id,
                            loader_target_id: &loader_target_id,
                            base_version_id: &base_version_id,
                            error,
                            observed_at: &observed_at,
                        },
                    )
                    .await;
                    let failure_summary = progress
                        .error
                        .clone()
                        .unwrap_or_else(|| BASE_INSTALL_FAILED_MESSAGE.to_string());
                    let _ = progress_tx.send(progress);
                    drop(progress_tx);
                    if finish_install_progress_task(store_task).await {
                        emit_install_failed(worker_telemetry.as_ref(), &failure_summary);
                    }
                }
                Ok(receipt) => {
                    let captured_terminal = final_progress
                        .lock()
                        .ok()
                        .and_then(|mut progress| progress.take());
                    let publication = publish_known_good_loader_terminal(
                        async {
                            require_exact_loader_receipt_version(
                                &version_id,
                                receipt.version_id(),
                            )?;
                            let (_, library_operation) = authority.as_ref().ok_or_else(|| {
                                std::io::Error::other(
                                    "managed install authority ended before receipt activation",
                                )
                            })?;
                            worker_state
                                .validate_managed_library_operation(library_operation)?;
                            worker_state
                                .accept_known_good_install_receipt(
                                    &loader_foreground,
                                    library_operation,
                                    receipt,
                                )
                                .await
                        },
                        captured_terminal,
                        |progress| {
                            let _ = progress_tx.send(progress);
                        },
                    )
                    .await;
                    if publication.acceptance_failed {
                        tracing::warn!(
                            operation_id = %worker_operation_id,
                            version_id = version_id.as_str(),
                            failure_kind = "known_good_reconciliation",
                            "loader install worker could not accept verified install authority"
                        );
                    }
                    drop(progress_tx);
                    let journal_committed = finish_install_progress_task(store_task).await;
                    if journal_committed && let Some(summary) = publication.failure_summary {
                        emit_install_failed(worker_telemetry.as_ref(), &summary);
                    }
                }
            }
            drop(authority);
        },
        move |progress| async move {
            let _foreground =
                retain_install_foreground(&interrupted_state, &interrupted_foreground).await;
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

    Ok(InstallStartResponse {
        install_id,
        operation_id,
        view_model: InstallProgressViewModel::starting(),
    })
}

pub(super) fn require_exact_loader_receipt_version(
    expected_version_id: &str,
    receipt_version_id: &str,
) -> std::io::Result<()> {
    if expected_version_id != receipt_version_id {
        return Err(std::io::Error::other(
            "verified loader receipt identity did not match the resolved install target",
        ));
    }
    Ok(())
}

pub(super) struct LoaderTerminalPublication {
    pub(super) acceptance_failed: bool,
    pub(super) failure_summary: Option<String>,
}

impl LoaderTerminalPublication {
    fn success() -> Self {
        Self {
            acceptance_failed: false,
            failure_summary: None,
        }
    }
}

pub(super) async fn publish_known_good_loader_terminal<F, P>(
    acceptance: F,
    captured_terminal: Option<DownloadProgress>,
    publish: P,
) -> LoaderTerminalPublication
where
    F: Future<Output = std::io::Result<()>>,
    P: FnOnce(DownloadProgress),
{
    match acceptance.await {
        Ok(()) => {
            publish(captured_terminal.unwrap_or_else(loader_install_done_progress));
            LoaderTerminalPublication::success()
        }
        Err(error) => {
            let error = known_good_acceptance_download_error(error);
            let progress = install_progress_with_terminal_error(
                terminal_failure_progress_or_default(captured_terminal),
                &error,
            );
            let sanitized = sanitize_install_progress(progress.clone());
            let failure_summary = sanitized
                .error
                .unwrap_or_else(|| INSTALL_FAILURE_MESSAGE.to_string());
            publish(progress);
            LoaderTerminalPublication {
                acceptance_failed: true,
                failure_summary: Some(failure_summary),
            }
        }
    }
}

pub(super) struct LoaderInstallFailureRequest<'a> {
    pub(super) operation_id: &'a crate::state::contracts::OperationId,
    pub(super) loader_target_id: &'a str,
    pub(super) base_version_id: &'a str,
    pub(super) error: LoaderInstallError,
    pub(super) observed_at: &'a str,
}

pub(super) async fn dispatch_loader_install_failure(
    producer: &ProducerLease,
    journals: Arc<crate::state::OperationJournalStore>,
    failure_memory: Arc<crate::state::GuardianFailureMemoryStore>,
    request: LoaderInstallFailureRequest<'_>,
) {
    let LoaderInstallFailureRequest {
        operation_id,
        loader_target_id,
        base_version_id,
        error,
        observed_at,
    } = request;
    match error {
        LoaderInstallError::BaseInstallFailed(failure) => {
            if failure.facts().is_empty() {
                record_loader_base_install_dependency_guardian_failure_outcome(
                    &journals,
                    operation_id,
                    loader_target_id,
                    base_version_id,
                )
                .await
                .ok();
                return;
            }
            record_install_failure_outcome_for_error(
                producer,
                journals,
                failure_memory,
                operation_id,
                failure.error(),
                failure.facts(),
                observed_at,
            )
            .await
        }
        LoaderInstallError::ArtifactDownloadFailed(failure) => {
            record_install_failure_outcome(
                producer,
                journals,
                failure_memory,
                operation_id,
                failure.facts(),
                observed_at,
            )
            .await
        }
        LoaderInstallError::Active(failure) => {
            record_loader_install_operation_guardian_failure_outcome(
                producer,
                journals,
                failure_memory,
                operation_id,
                loader_target_id,
                &failure,
                observed_at,
            )
            .await
            .ok();
        }
    }
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
    .map_err(loader_pre_operation_error_response)?;
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
        .map_err(loader_pre_operation_error_response)
}

pub(super) struct ObservedVanillaBaseInstall {
    store: Arc<InstallStore>,
    install_id: String,
    snapshot: InstallSnapshot,
    receiver: tokio::sync::broadcast::Receiver<InstallProgressRecord>,
}

pub(super) async fn observe_active_vanilla_base_install(
    store: &Arc<InstallStore>,
    version_id: &str,
) -> Result<Option<ObservedVanillaBaseInstall>, DownloadProgress> {
    let Some(install_id) = store.active_vanilla_install(version_id).await else {
        return Ok(None);
    };
    let Some((snapshot, receiver)) = store.subscribe_records(&install_id).await else {
        return Err(base_install_failed_progress());
    };
    if let Some(record) = snapshot.latest.as_ref()
        && let Some(terminal) = explicit_base_install_terminal(&record.progress)
    {
        return terminal.map(|()| None);
    }
    if snapshot.done {
        return Err(base_install_failed_progress());
    }
    Ok(Some(ObservedVanillaBaseInstall {
        store: store.clone(),
        install_id,
        snapshot,
        receiver,
    }))
}

pub(super) async fn wait_for_observed_vanilla_base_install(
    observed: ObservedVanillaBaseInstall,
    progress_tx: &mpsc::UnboundedSender<DownloadProgress>,
) -> Result<(), DownloadProgress> {
    let ObservedVanillaBaseInstall {
        store,
        install_id,
        snapshot,
        mut receiver,
    } = observed;
    debug_assert!(!snapshot.done);
    if let Some(record) = snapshot.latest {
        let _ = progress_tx.send(record.progress);
    }

    loop {
        match receiver.recv().await {
            Ok(record) => {
                if let Some(terminal) = explicit_base_install_terminal(&record.progress) {
                    return terminal;
                }
                let _ = progress_tx.send(record.progress);
            }
            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
            Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                let Some(snapshot) = store.snapshot(&install_id).await else {
                    return Err(base_install_failed_progress());
                };
                let Some(progress) = snapshot.latest.as_ref().map(|record| &record.progress) else {
                    return Err(base_install_failed_progress());
                };
                return explicit_base_install_terminal(progress)
                    .unwrap_or_else(|| Err(base_install_failed_progress()));
            }
        }
    }
}

fn explicit_base_install_terminal(
    progress: &DownloadProgress,
) -> Option<Result<(), DownloadProgress>> {
    progress.done.then(|| {
        if progress.error.is_some() {
            Err(base_install_failed_progress())
        } else {
            Ok(())
        }
    })
}

pub fn loader_pre_operation_error_response(error: LoaderError) -> InstallApplicationError {
    let failure_kind = error
        .pre_operation_failure_kind()
        .unwrap_or(LoaderPreOperationFailureKind::CatalogUnavailable);
    let copy_kind = if matches!(&error, LoaderError::CatalogUnavailable { .. }) {
        LoaderPreOperationFailureKind::CatalogUnavailable
    } else {
        failure_kind
    };
    let status = match failure_kind {
        LoaderPreOperationFailureKind::InvalidMinecraftVersion
        | LoaderPreOperationFailureKind::InvalidBuildId => StatusCode::BAD_REQUEST,
        LoaderPreOperationFailureKind::BuildNotFound => StatusCode::NOT_FOUND,
        LoaderPreOperationFailureKind::CatalogStale => StatusCode::PRECONDITION_FAILED,
        LoaderPreOperationFailureKind::ProviderHttpFailure
            if error.provider_failure_kind() == Some(LoaderProviderFailureKind::HttpNotFound) =>
        {
            StatusCode::NOT_FOUND
        }
        LoaderPreOperationFailureKind::CatalogUnavailable
        | LoaderPreOperationFailureKind::ProviderHttpFailure
        | LoaderPreOperationFailureKind::ProviderNetworkFailure
        | LoaderPreOperationFailureKind::ProviderRateLimited
        | LoaderPreOperationFailureKind::ProviderResponseTooLarge
        | LoaderPreOperationFailureKind::ProviderSchemaInvalid => StatusCode::BAD_GATEWAY,
    };
    (
        status,
        Json(serde_json::json!({
            "error": public_loader_pre_operation_error_message(copy_kind),
            "failure_kind": failure_kind,
        })),
    )
}

pub(crate) fn loader_install_error_progress(error: &LoaderInstallError) -> DownloadProgress {
    let progress = DownloadProgress {
        phase: "error".to_string(),
        current: 0,
        total: 0,
        file: None,
        error: Some(loader_install_error_message(error).to_string()),
        done: true,
        bytes_done: None,
        bytes_total: None,
    };
    if let LoaderInstallError::BaseInstallFailed(failure) = error {
        return install_progress_with_terminal_error(progress, failure.error());
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

fn public_loader_pre_operation_error_message(
    failure_kind: LoaderPreOperationFailureKind,
) -> &'static str {
    match failure_kind {
        LoaderPreOperationFailureKind::InvalidMinecraftVersion => "Invalid Minecraft version.",
        LoaderPreOperationFailureKind::InvalidBuildId => "Invalid loader build.",
        LoaderPreOperationFailureKind::CatalogUnavailable => {
            "Loader catalog is unavailable. Check your connection and try again."
        }
        LoaderPreOperationFailureKind::CatalogStale => {
            "Loader catalog needs a fresh provider check before this build can be installed."
        }
        LoaderPreOperationFailureKind::BuildNotFound => "Selected loader build is not available.",
        LoaderPreOperationFailureKind::ProviderHttpFailure
        | LoaderPreOperationFailureKind::ProviderNetworkFailure
        | LoaderPreOperationFailureKind::ProviderRateLimited => {
            "Loader provider is unavailable. Check your connection and try again."
        }
        LoaderPreOperationFailureKind::ProviderResponseTooLarge
        | LoaderPreOperationFailureKind::ProviderSchemaInvalid => {
            "Loader provider returned data Axial could not trust. Try again later."
        }
    }
}

fn loader_install_error_message(error: &LoaderInstallError) -> &'static str {
    match error {
        LoaderInstallError::BaseInstallFailed(_) => {
            "Base game install failed. Retry the install from Downloads."
        }
        LoaderInstallError::ArtifactDownloadFailed(_) => {
            "Loader download failed. Check your connection and try again."
        }
        LoaderInstallError::Active(failure) => active_loader_install_error_message(failure),
    }
}

fn active_loader_install_error_message(failure: &LoaderActiveInstallFailure) -> &'static str {
    match failure.kind() {
        LoaderInstallFailureKind::ArtifactMissing => {
            "Loader artifact is unavailable. Try another build or component."
        }
        LoaderInstallFailureKind::InvalidProfile => "Loader profile is invalid. Try another build.",
        LoaderInstallFailureKind::ProviderHttpFailure
        | LoaderInstallFailureKind::ProviderNetworkFailure
        | LoaderInstallFailureKind::ProviderRateLimited => {
            "Loader provider is unavailable. Check your connection and try again."
        }
        LoaderInstallFailureKind::ProviderResponseTooLarge
        | LoaderInstallFailureKind::ProviderSchemaInvalid => {
            "Loader provider returned data Axial could not trust. Try again later."
        }
        LoaderInstallFailureKind::VerifyFailed => {
            "Loader install verification failed. Try again or choose another build."
        }
        LoaderInstallFailureKind::ParseFailed => {
            "Loader install data could not be read. Try again."
        }
        LoaderInstallFailureKind::ProcessorFailed => {
            "Loader installer processor failed. Retry or choose another build."
        }
        LoaderInstallFailureKind::InstallExecutionFailed
            if matches!(
                failure.source(),
                LoaderError::Io(error) if error.kind() == std::io::ErrorKind::PermissionDenied
            ) =>
        {
            "Could not write loader files. Check app data permissions and try again."
        }
        LoaderInstallFailureKind::InstallExecutionFailed => {
            "Loader installer could not complete. Restart Axial and try again."
        }
    }
}

#[cfg(test)]
mod managed_install_settlement_tests {
    #[tokio::test]
    async fn loader_journal_failure_retains_mutation_until_install_settles() {
        super::super::managed_install_settlement_tests::assert_journal_failure_retains_mutation(
            "loader",
        )
        .await;
    }
}
