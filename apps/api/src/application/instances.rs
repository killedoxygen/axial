mod create;
mod create_cache;
mod create_policy;
mod resources;

#[cfg(test)]
pub(crate) use create::handle_create_instance;
pub(crate) use create::{
    CreateInstanceRequest, CreateInstanceResponse, CreateInstanceViewResponse,
    CreateLoaderBuildsViewResponse, handle_create_instance_owned, handle_create_instance_view,
    handle_create_loader_builds_view,
};
#[cfg(test)]
pub(crate) use create_cache::{
    create_view_cache_contains_root_for_tests, seed_create_view_cache_for_tests,
};
pub(crate) use create_cache::{invalidate_create_view_root, invalidate_create_view_source};

#[cfg(test)]
use create::{CreateSelection, resolve_loader_create_selection_from_build_catalog};

pub(crate) use resources::{
    InstanceLogInfo, InstanceLogTailResponse, InstanceModInfo, InstanceResourcesResponse,
    InstanceScreenshotInfo, InstanceWorldInfo, OpenFolderQuery, RenameScreenshotRequest,
    RenameWorldRequest, UpdateModRequest, WorldBackupResponse, handle_backup_instance_world,
    handle_delete_instance_mod, handle_delete_instance_screenshot, handle_delete_instance_world,
    handle_instance_log_tail, handle_instance_logs, handle_instance_mods,
    handle_instance_resources, handle_instance_screenshot_file, handle_instance_screenshots,
    handle_instance_worlds, handle_open_instance_folder, handle_rename_instance_screenshot,
    handle_rename_instance_world, handle_update_instance_mod,
};

#[cfg(test)]
use resources::{
    INSTANCE_LOG_READ_ERROR_MESSAGE, LOG_TAIL_LIMIT, SCREENSHOT_FILE_MAX_BYTES,
    WORLD_BACKUP_MAX_DEPTH, copy_world_backup_staged, instance_folder_open_error_response,
    instance_folder_prepare_error_response, instance_log_read_error_response,
    is_safe_resource_name, resolve_instance_folder, scan_instance_logs, screenshot_content_type,
    screenshot_file_read_error_response, screenshot_file_write_error_response, validate_mod_name,
    validate_screenshot_name, validate_world_name,
};

use crate::application::timing::{
    InstancesListTiming, trace_instances_list, trace_slow_instance_readiness,
};
use crate::application::version::{
    InstalledVersionsScan, VERSION_SCAN_DEGRADED_MESSAGE, VersionScanViewModel,
    installed_versions_scan,
};
use crate::guardian::normalize_create_jvm_preset;
use crate::state::{
    AppState, InstanceUpdate, IntegrityForegroundLease, KnownGoodRebuildError, ProducerLease,
    RequestProducerHandoff,
};
use axial_config::{EnrichedInstance, InstanceStoreError, LaunchActionState};
use axial_launcher::{
    GuardianMode, LaunchReadiness, LaunchReadinessReasonId, LaunchReadinessRequest,
    LaunchReadinessSeverity, inspect_launch_readiness_summary,
};
use axum::{Json, http::StatusCode};
use serde::{Deserialize, Serialize};
use std::{
    future::Future,
    io::ErrorKind,
    path::PathBuf,
    time::{Duration, Instant},
};
use tracing::error;

const INSTANCE_READINESS_SLOW_SPAN: Duration = Duration::from_millis(25);

#[derive(Clone, Copy)]
enum InstanceWriteOperation {
    Create,
    Duplicate,
    Update,
    Delete,
}

impl InstanceWriteOperation {
    fn internal_error_message(self) -> &'static str {
        match self {
            Self::Create => {
                "Could not create the instance. Check app data permissions and try again."
            }
            Self::Duplicate => {
                "Could not duplicate the instance. Check app data permissions and try again."
            }
            Self::Update => {
                "Could not save the instance. Check app data permissions and try again."
            }
            Self::Delete => {
                "Could not delete the instance. Check app data permissions and try again."
            }
        }
    }
}

fn instance_write_error_response(
    operation: InstanceWriteOperation,
    error: InstanceStoreError,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match error {
        InstanceStoreError::Read(error) | InstanceStoreError::Persistence(error) => {
            match error.kind() {
                ErrorKind::NotFound => (StatusCode::NOT_FOUND, "instance not found".to_string()),
                ErrorKind::AlreadyExists => (
                    StatusCode::CONFLICT,
                    "an instance with this name already exists".to_string(),
                ),
                ErrorKind::InvalidInput => (StatusCode::BAD_REQUEST, error.to_string()),
                ErrorKind::WouldBlock => (StatusCode::CONFLICT, error.to_string()),
                _ => (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    operation.internal_error_message().to_string(),
                ),
            }
        }
        InstanceStoreError::Validation(message) => (StatusCode::BAD_REQUEST, message.to_string()),
        InstanceStoreError::Parse(_) | InstanceStoreError::TooLarge { .. } => (
            StatusCode::INTERNAL_SERVER_ERROR,
            operation.internal_error_message().to_string(),
        ),
    };

    (status, Json(serde_json::json!({ "error": message })))
}

fn instance_internal_error_response(
    operation: InstanceWriteOperation,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": operation.internal_error_message() })),
    )
}

fn instance_shutdown_error_response<Error>(_error: Error) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "application shutdown is in progress" })),
    )
}

fn known_good_rebuild_error_response(
    operation: InstanceWriteOperation,
    error: KnownGoodRebuildError,
) -> (StatusCode, Json<serde_json::Value>) {
    let (status, message) = match error {
        KnownGoodRebuildError::LibraryRootUnavailable => (
            StatusCode::PRECONDITION_FAILED,
            "Axial library is not configured",
        ),
        KnownGoodRebuildError::CapacityExhausted => (
            StatusCode::SERVICE_UNAVAILABLE,
            "Version verification is busy. Try again.",
        ),
        KnownGoodRebuildError::ReconstructionFailed => (
            StatusCode::BAD_GATEWAY,
            "Could not verify the selected version. Check your connection and try again.",
        ),
        KnownGoodRebuildError::InstanceNotRegistered | KnownGoodRebuildError::TargetChanged => (
            StatusCode::CONFLICT,
            "The instance changed before version verification completed. Try again.",
        ),
        KnownGoodRebuildError::InvalidInstanceIdentity
        | KnownGoodRebuildError::ReceiptIdentityMismatch
        | KnownGoodRebuildError::LiveAuthorityMissing
        | KnownGoodRebuildError::OwnerStopped => {
            return instance_internal_error_response(operation);
        }
    };
    (status, Json(serde_json::json!({ "error": message })))
}

async fn rollback_new_instance(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    instance_id: &str,
) -> Result<(), InstanceStoreError> {
    match state
        .delete_instance(foreground, instance_id.to_string(), true)
        .await
    {
        Ok(()) => Ok(()),
        Err(_) if state.instances().get(instance_id).is_none() => Ok(()),
        Err(error) => Err(error),
    }
}

fn instance_store_error_class(error: &InstanceStoreError) -> &'static str {
    match error {
        InstanceStoreError::Read(_) => "read",
        InstanceStoreError::Parse(_) => "parse",
        InstanceStoreError::Validation(_) => "validation",
        InstanceStoreError::TooLarge { .. } => "too_large",
        InstanceStoreError::Persistence(_) => "persistence",
    }
}

#[derive(Debug, Serialize)]
pub(crate) struct InstancesResponse {
    pub instances: Vec<EnrichedInstance>,
    pub last_instance_id: Option<String>,
    pub scan_state: VersionScanViewModel,
}

pub(crate) async fn handle_list_instances(
    state: &AppState,
    producer: &ProducerLease,
) -> InstancesResponse {
    let started_at = Instant::now();
    let scan_started_at = Instant::now();
    let (scan, scan_source, refresh_count, library_dir) =
        indexed_current_versions(state, producer).await;
    let scan_elapsed = scan_started_at.elapsed();

    let enrich_started_at = Instant::now();
    let instances = enrich_instances_for_state(state, &scan, library_dir.as_deref());
    let enrich_elapsed = enrich_started_at.elapsed();

    trace_instances_list(InstancesListTiming {
        total: started_at.elapsed(),
        scan: scan_elapsed,
        enrich: enrich_elapsed,
        version_count: scan.versions.len(),
        instance_count: instances.len(),
        degraded: scan.is_degraded(),
        scan_source,
        refresh_count,
    });

    InstancesResponse {
        instances,
        last_instance_id: state.instances().last_instance_id(),
        scan_state: scan.view_model,
    }
}

async fn indexed_current_versions(
    state: &AppState,
    producer: &ProducerLease,
) -> (InstalledVersionsScan, &'static str, u32, Option<PathBuf>) {
    let Some(lookup) = state.installed_versions_snapshot(producer).await else {
        return (unconfigured_versions_scan(), "unconfigured", 0, None);
    };
    let source = lookup.source.as_str();
    let refresh_count = lookup.refresh_count;
    let library_dir = lookup.library_dir().to_path_buf();
    (
        installed_versions_scan(&lookup.snapshot),
        source,
        refresh_count,
        Some(library_dir),
    )
}

fn unconfigured_versions_scan() -> InstalledVersionsScan {
    InstalledVersionsScan {
        versions: Vec::new(),
        view_model: VersionScanViewModel {
            state_id: "library_unconfigured".to_string(),
            label: "Library is not configured".to_string(),
            degraded: false,
            detail: None,
        },
    }
}

async fn enrich_instance_for_state_indexed(
    state: &AppState,
    producer: &ProducerLease,
    instance: axial_config::Instance,
) -> EnrichedInstance {
    let (scan, _, _, library_dir) = indexed_current_versions(state, producer).await;
    enrich_instance_for_scan(state, instance, &scan, library_dir.as_deref())
}

async fn enrich_instance_for_state_with_foreground(
    state: &AppState,
    foreground: &IntegrityForegroundLease,
    producer: &ProducerLease,
    instance: axial_config::Instance,
) -> EnrichedInstance {
    let Some(lookup) = state
        .installed_versions_snapshot_with_foreground(producer, foreground.retained())
        .await
    else {
        return enrich_instance_for_scan(state, instance, &unconfigured_versions_scan(), None);
    };
    let library_dir = lookup.library_dir().to_path_buf();
    let scan = installed_versions_scan(&lookup.snapshot);
    enrich_instance_for_scan(state, instance, &scan, Some(&library_dir))
}

fn enrich_instances_for_state(
    state: &AppState,
    scan: &InstalledVersionsScan,
    library_dir: Option<&std::path::Path>,
) -> Vec<EnrichedInstance> {
    state
        .instances()
        .list()
        .into_iter()
        .map(|instance| enrich_instance_for_scan(state, instance, scan, library_dir))
        .collect()
}

pub(super) fn enrich_instance_for_scan(
    state: &AppState,
    instance: axial_config::Instance,
    scan: &InstalledVersionsScan,
    library_dir: Option<&std::path::Path>,
) -> EnrichedInstance {
    let version = scan
        .versions
        .iter()
        .find(|version| version.id == instance.version_id);
    let mut enriched = redact_runtime_overrides(
        EnrichedInstance::from_instance_without_resource_counts(instance.clone(), version),
    );
    if scan.is_degraded() {
        apply_blocked_launch_action(&mut enriched, VERSION_SCAN_DEGRADED_MESSAGE);
        return enriched;
    }

    let Some(library_dir) = library_dir else {
        return enriched;
    };
    let config = state.config().current();
    let readiness_started_at = Instant::now();
    let readiness = inspect_launch_readiness_summary(
        state.managed_runtime_cache(),
        &LaunchReadinessRequest {
            library_dir: library_dir.to_path_buf(),
            requested_java: selected_java_override(&instance, &config),
            version_id: instance.version_id.clone(),
            guardian_mode: GuardianMode::from_config(&config.guardian_mode),
        },
    );
    let readiness_elapsed = readiness_started_at.elapsed();
    if readiness_elapsed >= INSTANCE_READINESS_SLOW_SPAN {
        trace_slow_instance_readiness(
            &instance.id,
            &instance.version_id,
            readiness_elapsed,
            readiness.launchable,
            readiness.reasons.len(),
        );
    }
    apply_launch_readiness(&mut enriched, &readiness);
    enriched
}

fn selected_java_override(
    instance: &axial_config::Instance,
    config: &axial_config::AppConfig,
) -> String {
    if !instance.java_path.trim().is_empty() {
        instance.java_path.trim().to_string()
    } else {
        config.java_path_override.trim().to_string()
    }
}

fn apply_launch_readiness(instance: &mut EnrichedInstance, readiness: &LaunchReadiness) {
    if readiness.launchable {
        instance.launchable = true;
        instance.launch_action = LaunchActionState::launch_ready();
        return;
    }

    let detail = readiness_blocking_message(readiness);
    instance.launchable = false;
    instance.status_detail = detail.clone();
    if instance.needs_install.trim().is_empty() {
        instance.needs_install = instance.version_id.clone();
    }
    instance.launch_action = if readiness_has_corrupt_managed_artifact(readiness) {
        LaunchActionState::repair_required(detail)
    } else if readiness_requires_launcher_install(readiness) {
        LaunchActionState::install_required(detail)
    } else if readiness_is_user_blocked(readiness) {
        instance.needs_install.clear();
        LaunchActionState::blocked(detail)
    } else {
        LaunchActionState::install_required(detail)
    };
}

fn apply_blocked_launch_action(instance: &mut EnrichedInstance, detail: &str) {
    instance.launchable = false;
    instance.status_detail = detail.to_string();
    instance.needs_install.clear();
    instance.launch_action = LaunchActionState::blocked(detail.to_string());
}

fn readiness_blocking_message(readiness: &LaunchReadiness) -> String {
    readiness
        .reasons
        .iter()
        .find(|reason| reason.severity == LaunchReadinessSeverity::Blocking)
        .or_else(|| readiness.reasons.first())
        .map(|reason| reason.message.to_string())
        .unwrap_or_else(|| "Version files are not ready.".to_string())
}

fn readiness_has_corrupt_managed_artifact(readiness: &LaunchReadiness) -> bool {
    readiness.reasons.iter().any(|reason| {
        matches!(
            reason.id,
            LaunchReadinessReasonId::ClientJarCorrupt
                | LaunchReadinessReasonId::LibrariesCorrupt
                | LaunchReadinessReasonId::AssetIndexCorrupt
        )
    })
}

fn readiness_requires_launcher_install(readiness: &LaunchReadiness) -> bool {
    readiness.reasons.iter().any(|reason| {
        matches!(
            reason.id,
            LaunchReadinessReasonId::VersionJsonMissing
                | LaunchReadinessReasonId::ParentVersionMissing
                | LaunchReadinessReasonId::IncompleteInstall
                | LaunchReadinessReasonId::ClientJarMissing
                | LaunchReadinessReasonId::LibrariesMissing
                | LaunchReadinessReasonId::AssetIndexMissing
                | LaunchReadinessReasonId::ManagedRuntimeMissing
        )
    })
}

fn readiness_is_user_blocked(readiness: &LaunchReadiness) -> bool {
    readiness
        .reasons
        .iter()
        .any(|reason| reason.id == LaunchReadinessReasonId::JavaOverrideMissing)
}

pub(crate) async fn handle_get_instance(
    state: &AppState,
    producer: &ProducerLease,
    id: &str,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(id);

    match instance {
        Some(instance) => Ok(enrich_instance_for_state_indexed(state, producer, instance).await),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )),
    }
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct DuplicateInstanceRequest {
    pub name: Option<String>,
}

pub(crate) async fn handle_duplicate_instance_owned(
    state: &AppState,
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
    handoff: RequestProducerHandoff,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    handle_duplicate_instance_owned_with_rebuild(
        state,
        id,
        payload,
        handoff,
        |state, foreground, producer, instance_id| async move {
            crate::application::rebuild_registered_known_good(
                &state,
                &foreground,
                &producer,
                &instance_id,
            )
            .await
        },
    )
    .await
}

async fn handle_duplicate_instance_owned_with_rebuild<Rebuild, RebuildFuture>(
    state: &AppState,
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
    handoff: RequestProducerHandoff,
    rebuild: Rebuild,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)>
where
    Rebuild: FnOnce(AppState, IntegrityForegroundLease, ProducerLease, String) -> RebuildFuture
        + Send
        + 'static,
    RebuildFuture: Future<Output = Result<(), KnownGoodRebuildError>> + Send + 'static,
{
    let producer = handoff
        .try_claim()
        .map_err(instance_shutdown_error_response)?;
    let foreground = state
        .register_integrity_foreground()
        .map_err(instance_shutdown_error_response)?;
    let transaction = producer.claim_child();
    let rebuild_owner = producer.claim_child();
    let enrich_owner = producer.claim_child();
    let transaction_state = state.clone();
    let source_id = id.to_string();
    let completed = transaction.spawn_joinable(async move {
        let foreground = foreground.wait_for_settlement().await;
        let payload = payload.unwrap_or_default();
        let instance = transaction_state
            .duplicate_instance(
                &foreground,
                source_id,
                payload.name,
                transaction_state.library_dir().map(PathBuf::from),
            )
            .await
            .map_err(|error| {
                instance_write_error_response(InstanceWriteOperation::Duplicate, error)
            })?;
        let instance_id = instance.id.clone();
        match rebuild(
            transaction_state.clone(),
            foreground.retained(),
            rebuild_owner,
            instance_id.clone(),
        )
        .await
        {
            Ok(()) => Ok(enrich_instance_for_state_with_foreground(
                &transaction_state,
                &foreground,
                &enrich_owner,
                instance,
            )
            .await),
            Err(error) => {
                if let Err(rollback_error) =
                    rollback_new_instance(&transaction_state, &foreground, &instance_id).await
                {
                    error!(
                        failure_class = instance_store_error_class(&rollback_error),
                        "duplicate compensation rollback persistence failed"
                    );
                    return Err(instance_write_error_response(
                        InstanceWriteOperation::Duplicate,
                        rollback_error,
                    ));
                }
                Err(known_good_rebuild_error_response(
                    InstanceWriteOperation::Duplicate,
                    error,
                ))
            }
        }
    });
    completed
        .await
        .map_err(|_| instance_internal_error_response(InstanceWriteOperation::Duplicate))?
}

#[cfg(test)]
pub(crate) async fn handle_duplicate_instance(
    state: &AppState,
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let request = state
        .try_admit_request()
        .expect("admit test duplicate request");
    handle_duplicate_instance_owned_with_rebuild(
        state,
        id,
        payload,
        request.producer_handoff(),
        |_, _, _, _| async { Ok(()) },
    )
    .await
}

#[cfg(test)]
async fn handle_duplicate_instance_with_rebuild<Rebuild, RebuildFuture>(
    state: &AppState,
    id: &str,
    payload: Option<DuplicateInstanceRequest>,
    handoff: RequestProducerHandoff,
    rebuild: Rebuild,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)>
where
    Rebuild: FnOnce(AppState, IntegrityForegroundLease, ProducerLease, String) -> RebuildFuture
        + Send
        + 'static,
    RebuildFuture: Future<Output = Result<(), KnownGoodRebuildError>> + Send + 'static,
{
    handle_duplicate_instance_owned_with_rebuild(state, id, payload, handoff, rebuild).await
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct InstancePatch {
    pub name: Option<String>,
    pub version_id: Option<String>,
    pub art_seed: Option<u32>,
    pub max_memory_mb: Option<i32>,
    pub min_memory_mb: Option<i32>,
    pub java_path: Option<String>,
    pub window_width: Option<i32>,
    pub window_height: Option<i32>,
    pub jvm_preset: Option<String>,
    pub performance_mode: Option<String>,
    pub extra_jvm_args: Option<String>,
    pub icon: Option<String>,
    pub accent: Option<String>,
}

pub(crate) async fn handle_update_instance_owned(
    state: &AppState,
    id: &str,
    patch: InstancePatch,
    handoff: RequestProducerHandoff,
) -> Result<EnrichedInstance, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(instance_shutdown_error_response)?;
    let foreground = state
        .register_integrity_foreground()
        .map_err(instance_shutdown_error_response)?;
    let update = InstanceUpdate {
        name: patch.name,
        expected_version_id: patch.version_id,
        art_seed: patch.art_seed,
        max_memory_mb: patch.max_memory_mb,
        min_memory_mb: patch.min_memory_mb,
        java_path: patch.java_path,
        window_width: patch.window_width,
        window_height: patch.window_height,
        jvm_preset: patch.jvm_preset.map(|value| {
            normalize_create_jvm_preset(Some(&value))
                .stored_preset()
                .to_string()
        }),
        performance_mode: patch.performance_mode,
        extra_jvm_args: patch.extra_jvm_args,
        icon: patch.icon,
        accent: patch.accent,
    };
    let transaction = producer.claim_child();
    let enrich_owner = producer.claim_child();
    let transaction_state = state.clone();
    let instance_id = id.to_string();
    transaction
        .spawn_joinable(async move {
            let foreground = foreground.wait_for_settlement().await;
            let instance = transaction_state
                .update_instance(&foreground, instance_id, update)
                .await
                .map_err(|error| {
                    instance_write_error_response(InstanceWriteOperation::Update, error)
                })?;
            let enriched = enrich_instance_for_state_with_foreground(
                &transaction_state,
                &foreground,
                &enrich_owner,
                instance,
            )
            .await;
            drop(foreground);
            Ok(enriched)
        })
        .await
        .map_err(|_| instance_internal_error_response(InstanceWriteOperation::Update))?
}

fn redact_runtime_overrides(mut instance: EnrichedInstance) -> EnrichedInstance {
    instance.instance.java_path.clear();
    instance.instance.extra_jvm_args.clear();
    instance
}

pub(crate) async fn handle_delete_instance_owned(
    state: &AppState,
    id: &str,
    query: std::collections::HashMap<String, String>,
    handoff: RequestProducerHandoff,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(instance_shutdown_error_response)?;
    let foreground = state
        .register_integrity_foreground()
        .map_err(instance_shutdown_error_response)?;
    let keep_files = query.get("keep_files").is_some_and(|value| value == "true");
    let transaction = producer.claim_child();
    let transaction_state = state.clone();
    let instance_id = id.to_string();
    transaction
        .spawn_joinable(async move {
            let foreground = foreground.wait_for_settlement().await;
            transaction_state
                .delete_instance(&foreground, instance_id, !keep_files)
                .await
                .map_err(|error| {
                    instance_write_error_response(InstanceWriteOperation::Delete, error)
                })?;
            Ok(serde_json::json!({ "status": "ok" }))
        })
        .await
        .map_err(|_| instance_internal_error_response(InstanceWriteOperation::Delete))?
}

#[cfg(test)]
pub(crate) async fn handle_delete_instance(
    state: &AppState,
    id: &str,
    query: std::collections::HashMap<String, String>,
) -> Result<serde_json::Value, (StatusCode, Json<serde_json::Value>)> {
    let request = state
        .try_admit_request()
        .expect("admit test delete request");
    handle_delete_instance_owned(state, id, query, request.producer_handoff()).await
}

#[cfg(test)]
mod tests;
