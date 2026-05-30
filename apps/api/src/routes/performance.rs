use crate::state::performance_operations::{
    PerformanceOperationConflict, PerformanceOperationStatus,
};
use crate::state::{AppState, DownloadProgress};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_performance::InstallError;
use croopor_performance::{
    BundleHealth, CompositionPlan, CompositionTier, OwnershipClass, PerformanceMode,
    PerformanceRulesStatus, ResolutionRequest, RollbackSnapshotSummary, RulesRefreshError,
    StateError, derive_health, extract_base_version, infer_loader_from_version_id, load_state,
    parse_mode,
};
use serde::{Deserialize, Serialize};

const PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT: usize = 50;

#[derive(Debug, Deserialize)]
struct PlanQuery {
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthQuery {
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RollbackQuery {
    instance_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    instance_id: Option<String>,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: Option<String>,
    rollback_id: Option<String>,
    queued: Option<bool>,
}

#[derive(Debug, Serialize)]
struct PerformancePlanResponse {
    active: bool,
    #[serde(flatten)]
    plan: CompositionPlan,
}

#[derive(Debug, Serialize)]
struct PerformanceHealthResponse {
    active: bool,
    health: BundleHealth,
    composition_id: String,
    tier: String,
    installed_count: usize,
    managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    warnings: Vec<String>,
}

#[derive(Debug, Serialize)]
struct PerformanceInstallResponse {
    active: bool,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    install_id: Option<String>,
    health: BundleHealth,
    composition_id: String,
    tier: String,
    installed_count: usize,
    managed_artifacts: Vec<PerformanceManagedArtifactSummary>,
    warnings: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PerformanceManagedArtifactSummary {
    project_id: String,
    version_id: String,
    filename: String,
    ownership_class: OwnershipClass,
    sha512_present: bool,
}

#[derive(Debug, Serialize)]
struct PerformanceRollbackListResponse {
    snapshots: Vec<RollbackSnapshotSummary>,
}

#[derive(Debug, Clone)]
struct PerformanceOperation {
    instance_id: String,
    version_id: String,
    instance_performance_mode: String,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: PerformanceInstallAction,
    rollback_id: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/performance/status", get(handle_status))
        .route(
            "/api/v1/performance/rules/refresh",
            post(handle_rules_refresh),
        )
        .route("/api/v1/performance/plan", get(handle_plan))
        .route("/api/v1/performance/health", get(handle_health))
        .route("/api/v1/performance/rollback", get(handle_rollback_list))
        .route("/api/v1/performance/install", post(handle_install))
        .route(
            "/api/v1/performance/operations/{id}",
            get(handle_operation_status),
        )
}

async fn handle_status(
    State(state): State<AppState>,
) -> Result<Json<PerformanceRulesStatus>, (StatusCode, Json<serde_json::Value>)> {
    Ok(Json(state.performance().rules_status()))
}

async fn handle_rules_refresh(
    State(state): State<AppState>,
) -> Result<Json<PerformanceRulesStatus>, (StatusCode, Json<serde_json::Value>)> {
    match state.performance().refresh_rules().await {
        Ok(status) => Ok(Json(status)),
        Err(RulesRefreshError::Unconfigured) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "performance remote rules url is not configured"
            })),
        )),
        Err(error) => Err(internal_error(error)),
    }
}

async fn handle_plan(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<PerformancePlanResponse>, (StatusCode, Json<serde_json::Value>)> {
    let game_version = required_value(
        query.game_version.as_deref(),
        "game_version query parameter is required",
    )?;
    let mode = resolve_config_mode(&state, query.mode.as_deref())?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader: optional_value(query.loader.as_deref()).unwrap_or_default(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: Vec::new(),
    });

    Ok(Json(PerformancePlanResponse {
        active: matches!(mode, PerformanceMode::Managed),
        plan,
    }))
}

async fn handle_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<PerformanceHealthResponse>, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(
        query.instance_id.as_deref(),
        "instance_id query parameter is required",
    )?;
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mode = resolve_instance_mode(&state, &instance.performance_mode, None)?;

    if !matches!(mode, PerformanceMode::Managed) {
        return Ok(Json(disabled_health_response()));
    }

    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = match load_state(&mods_dir) {
        Ok(state_file) => state_file,
        Err(StateError::Parse(error)) => {
            return Ok(Json(PerformanceHealthResponse {
                active: true,
                health: BundleHealth::Invalid,
                composition_id: String::new(),
                tier: String::new(),
                installed_count: 0,
                managed_artifacts: Vec::new(),
                warnings: vec![format!("failed to parse performance state: {error}")],
            }));
        }
        Err(StateError::InvalidOwnership { .. }) => {
            return Ok(Json(PerformanceHealthResponse {
                active: true,
                health: BundleHealth::Invalid,
                composition_id: String::new(),
                tier: String::new(),
                installed_count: 0,
                managed_artifacts: Vec::new(),
                warnings: vec!["invalid performance artifact ownership metadata".to_string()],
            }));
        }
        Err(error) => return Err(internal_error(error)),
    };
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: extract_base_version(&instance.version_id),
        loader: infer_loader_from_version_id(&instance.version_id),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_ids_from_state(state_file.as_ref()),
    });
    let (health, warnings) = derive_health(state_file.as_ref(), Some(&plan), &mods_dir);

    Ok(Json(PerformanceHealthResponse {
        active: true,
        health,
        composition_id: state_file
            .as_ref()
            .map(|value| value.composition_id.clone())
            .unwrap_or_default(),
        tier: state_file
            .as_ref()
            .map(|value| tier_name(value.tier).to_string())
            .unwrap_or_default(),
        installed_count: state_file
            .as_ref()
            .map(|value| value.installed_mods.len())
            .unwrap_or_default(),
        managed_artifacts: managed_artifact_summary(state_file.as_ref()),
        warnings,
    }))
}

async fn handle_rollback_list(
    State(state): State<AppState>,
    Query(query): Query<RollbackQuery>,
) -> Result<Json<PerformanceRollbackListResponse>, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(
        query.instance_id.as_deref(),
        "instance_id query parameter is required",
    )?;
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let snapshots = state
        .performance()
        .list_rollback_snapshots(&mods_dir)
        .map_err(performance_install_error)?;

    Ok(Json(PerformanceRollbackListResponse { snapshots }))
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<PerformanceInstallResponse>, (StatusCode, Json<serde_json::Value>)> {
    let instance_id = required_value(payload.instance_id.as_deref(), "instance_id is required")?;
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let action = install_action(payload.action.as_deref())?;
    let operation = PerformanceOperation {
        instance_id: instance.id.clone(),
        version_id: instance.version_id.clone(),
        instance_performance_mode: instance.performance_mode.clone(),
        game_version: payload.game_version.clone(),
        loader: payload.loader.clone(),
        mode: payload.mode.clone(),
        action,
        rollback_id: payload.rollback_id.clone(),
    };

    if payload.queued.unwrap_or(false) {
        return queue_performance_operation(state, operation)
            .await
            .map(Json);
    }

    execute_performance_operation(&state, &operation)
        .await
        .map(Json)
}

async fn handle_operation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<PerformanceOperationStatus>, (StatusCode, Json<serde_json::Value>)> {
    state
        .performance_operations()
        .get(&id)
        .await
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "performance operation not found" })),
            )
        })
}

async fn execute_performance_operation(
    state: &AppState,
    operation: &PerformanceOperation,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let performance = state.performance().clone();
    let mods_dir = state
        .instances()
        .game_dir(&operation.instance_id)
        .join("mods");

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        let restored_state =
            if let Some(rollback_id) = optional_value(operation.rollback_id.as_deref()) {
                performance
                    .rollback_managed_snapshot(&mods_dir, &rollback_id)
                    .map_err(performance_install_error)?
            } else {
                performance
                    .rollback_managed(&mods_dir)
                    .map_err(performance_install_error)?
            };
        let (health, warnings) = derive_health(Some(&restored_state), None, &mods_dir);

        return Ok(PerformanceInstallResponse {
            active: true,
            status: "rolled_back".to_string(),
            install_id: None,
            health,
            composition_id: restored_state.composition_id.clone(),
            tier: tier_name(restored_state.tier).to_string(),
            installed_count: restored_state.installed_mods.len(),
            managed_artifacts: managed_artifact_summary(Some(&restored_state)),
            warnings,
        });
    }

    let mode = resolve_instance_mode(
        state,
        &operation.instance_performance_mode,
        operation.mode.as_deref(),
    )?;

    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        performance
            .remove_managed(&mods_dir)
            .map_err(performance_install_error)?;

        return Ok(removed_install_response());
    }

    let game_version = operation
        .game_version
        .as_deref()
        .and_then(|value| optional_value(Some(value)))
        .unwrap_or_else(|| extract_base_version(&operation.version_id));
    let loader = operation
        .loader
        .as_deref()
        .and_then(|value| optional_value(Some(value)))
        .unwrap_or_else(|| infer_loader_from_version_id(&operation.version_id));
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: game_version.clone(),
        loader: loader.clone(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: Vec::new(),
    });
    let installed_state = performance
        .ensure_installed(&plan, &game_version, &mods_dir)
        .await
        .map_err(performance_install_error)?;
    let (health, warnings) = derive_health(Some(&installed_state), Some(&plan), &mods_dir);

    Ok(PerformanceInstallResponse {
        active: true,
        status: "complete".to_string(),
        install_id: None,
        health,
        composition_id: installed_state.composition_id.clone(),
        tier: tier_name(installed_state.tier).to_string(),
        installed_count: installed_state.installed_mods.len(),
        managed_artifacts: managed_artifact_summary(Some(&installed_state)),
        warnings,
    })
}

async fn queue_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let status = state
        .performance_operations()
        .start(
            operation.instance_id.clone(),
            operation_action_name(operation.action).to_string(),
        )
        .await
        .map_err(performance_operation_conflict)?;
    let install_id = status.id.clone();
    state.installs().insert(install_id.clone()).await;

    let store = state.installs().clone();
    let install_id_task = install_id.clone();
    tokio::spawn(async move {
        run_queued_performance_operation(state, operation, store, install_id_task).await;
    });

    Ok(PerformanceInstallResponse {
        active: true,
        status: "queued".to_string(),
        install_id: Some(install_id),
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
    })
}

async fn run_queued_performance_operation(
    state: AppState,
    operation: PerformanceOperation,
    store: std::sync::Arc<crate::state::InstallStore>,
    install_id: String,
) {
    state
        .performance_operations()
        .record_progress(&install_id, "queued")
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        "queued",
        0,
        4,
        Some("Queued performance update"),
        None,
        false,
    )
    .await;
    state
        .performance_operations()
        .record_progress(&install_id, "planning")
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        "planning",
        1,
        4,
        Some("Planning performance bundle"),
        None,
        false,
    )
    .await;
    state
        .performance_operations()
        .record_progress(&install_id, operation_progress_phase(operation.action))
        .await;
    emit_performance_progress(
        &store,
        &install_id,
        operation_progress_phase(operation.action),
        2,
        4,
        Some(operation_progress_label(operation.action)),
        None,
        false,
    )
    .await;

    match execute_performance_operation(&state, &operation).await {
        Ok(response) => {
            state
                .performance_operations()
                .record_complete(&install_id)
                .await;
            emit_performance_progress(
                &store,
                &install_id,
                "complete",
                4,
                4,
                Some(complete_progress_label(&response.status)),
                None,
                true,
            )
            .await;
        }
        Err(error) => {
            let message = error_message(&error);
            state
                .performance_operations()
                .record_failed(&install_id, &message)
                .await;
            emit_performance_progress(
                &store,
                &install_id,
                "error",
                4,
                4,
                None,
                Some(message),
                true,
            )
            .await;
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn emit_performance_progress(
    store: &crate::state::InstallStore,
    install_id: &str,
    phase: &str,
    current: i32,
    total: i32,
    file: Option<&str>,
    error: Option<String>,
    done: bool,
) {
    store
        .emit(
            install_id,
            DownloadProgress {
                phase: phase.to_string(),
                current,
                total,
                file: file.map(ToOwned::to_owned),
                error,
                done,
            },
        )
        .await;
}

fn operation_progress_phase(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "applying",
        PerformanceInstallAction::Remove => "removing",
        PerformanceInstallAction::Rollback => "rolling_back",
    }
}

fn operation_progress_label(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "Applying managed performance files",
        PerformanceInstallAction::Remove => "Removing managed performance files",
        PerformanceInstallAction::Rollback => "Rolling back managed performance files",
    }
}

fn operation_action_name(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "install",
        PerformanceInstallAction::Remove => "remove",
        PerformanceInstallAction::Rollback => "rollback",
    }
}

fn complete_progress_label(status: &str) -> &'static str {
    match status {
        "removed" => "Managed performance files removed",
        "rolled_back" => "Managed performance files rolled back",
        _ => "Managed performance bundle updated",
    }
}

fn error_message(error: &(StatusCode, Json<serde_json::Value>)) -> String {
    error
        .1
        .0
        .get("error")
        .and_then(|value| value.as_str())
        .unwrap_or("performance operation failed")
        .to_string()
}

fn managed_artifact_summary(
    state: Option<&croopor_performance::CompositionState>,
) -> Vec<PerformanceManagedArtifactSummary> {
    state
        .map(|state| {
            state
                .installed_mods
                .iter()
                .take(PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT)
                .map(|installed| PerformanceManagedArtifactSummary {
                    project_id: installed.project_id.clone(),
                    version_id: installed.version_id.clone(),
                    filename: installed.filename.clone(),
                    ownership_class: installed.ownership_class,
                    sha512_present: !installed.sha512.trim().is_empty(),
                })
                .collect()
        })
        .unwrap_or_default()
}

fn disabled_health_response() -> PerformanceHealthResponse {
    PerformanceHealthResponse {
        active: false,
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
    }
}

fn removed_install_response() -> PerformanceInstallResponse {
    PerformanceInstallResponse {
        active: false,
        status: "removed".to_string(),
        install_id: None,
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PerformanceInstallAction {
    Install,
    Remove,
    Rollback,
}

fn install_action(
    raw: Option<&str>,
) -> Result<PerformanceInstallAction, (StatusCode, Json<serde_json::Value>)> {
    match optional_value(raw).as_deref() {
        None | Some("install") | Some("apply") => Ok(PerformanceInstallAction::Install),
        Some("remove") | Some("disable") => Ok(PerformanceInstallAction::Remove),
        Some("rollback") => Ok(PerformanceInstallAction::Rollback),
        Some(value) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid performance action: {value}") })),
        )),
    }
}

fn required_value(
    raw: Option<&str>,
    message: &str,
) -> Result<String, (StatusCode, Json<serde_json::Value>)> {
    optional_value(raw).ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": message })),
        )
    })
}

fn optional_value(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
}

fn tier_name(tier: CompositionTier) -> &'static str {
    match tier {
        CompositionTier::Extended => "extended",
        CompositionTier::Core => "core",
        CompositionTier::VanillaEnhanced => "vanilla_enhanced",
    }
}

fn resolve_config_mode(
    state: &AppState,
    raw: Option<&str>,
) -> Result<PerformanceMode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        return parse_mode(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid performance mode: {raw}") })),
            )
        });
    }
    Ok(parse_mode(&state.config().current().performance_mode).unwrap_or(PerformanceMode::Managed))
}

fn resolve_instance_mode(
    state: &AppState,
    instance_mode: &str,
    raw: Option<&str>,
) -> Result<PerformanceMode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        return parse_mode(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": format!("invalid performance mode: {raw}") })),
            )
        });
    }
    if let Some(mode) = parse_mode(instance_mode) {
        return Ok(mode);
    }
    resolve_config_mode(state, None)
}

fn installed_mod_ids_from_state(
    state: Option<&croopor_performance::CompositionState>,
) -> Vec<String> {
    state
        .map(|value| {
            value
                .installed_mods
                .iter()
                .map(|installed| installed.project_id.clone())
                .collect()
        })
        .unwrap_or_default()
}

fn internal_error(error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": error.to_string() })),
    )
}

fn performance_install_error(error: InstallError) -> (StatusCode, Json<serde_json::Value>) {
    match error {
        InstallError::NoRollbackSnapshot | InstallError::RollbackSnapshotNotFound => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": error.to_string() })),
        ),
        InstallError::State(StateError::InvalidRollbackId) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid performance rollback snapshot id" })),
        ),
        InstallError::State(StateError::InvalidOwnership { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact ownership metadata"
            })),
        ),
        error => internal_error(error),
    }
}

fn performance_operation_conflict(
    _error: PerformanceOperationConflict,
) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({
            "error": "a performance operation is already queued for this instance"
        })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, SessionStore};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::{CompositionState, InstalledMod, PerformanceManager};
    use std::{fs, path::PathBuf, sync::Arc, time::Duration};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    #[tokio::test]
    async fn status_reports_bundled_rules_without_remote_refresh() {
        let fixture = TestFixture::new("status");

        let Json(response) = handle_status(State(fixture.state.clone()))
            .await
            .expect("status should serialize");

        assert_eq!(
            response.rule_source,
            croopor_performance::RuleSource::BuiltIn
        );
        assert_eq!(
            response.rule_channel,
            croopor_performance::RuleChannel::Bundled
        );
        assert!(response.rules_cache.recorded);
        assert_eq!(
            response.rules_cache.state,
            croopor_performance::RulesCacheState::Recorded
        );
        assert!(response.rules_cache.updated_at.is_some());
        assert!(response.rules_cache.loaded_at.is_some());
        assert!(response.rules_cache.warning.is_none());
        assert_eq!(response.schema_version, 1);
        assert!(!response.generated_at.is_empty());
        assert!(response.composition_count > 0);
        assert!(!response.remote_refresh);
        assert_eq!(response.last_refresh_at, None);
        assert_eq!(
            response.validation,
            croopor_performance::RulesValidation::Valid
        );
        assert_eq!(
            response.health_states,
            vec![
                BundleHealth::Healthy,
                BundleHealth::Degraded,
                BundleHealth::Fallback,
                BundleHealth::Disabled,
                BundleHealth::Invalid,
            ]
        );
        assert_eq!(
            response.ownership_classes,
            vec![
                croopor_performance::OwnershipClass::CompositionManaged,
                croopor_performance::OwnershipClass::UserManaged,
            ]
        );
        assert!(response.warnings.is_empty());
    }

    #[tokio::test]
    async fn rules_refresh_route_requires_configured_remote_url() {
        let fixture = TestFixture::new("rules-refresh-unconfigured");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/performance/rules/refresh")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("error json");
        assert_eq!(
            value,
            serde_json::json!({ "error": "performance remote rules url is not configured" })
        );
    }

    #[tokio::test]
    async fn rules_refresh_route_accepts_configured_remote_manifest() {
        let mut manifest = croopor_performance::builtin_manifest().expect("builtin manifest");
        manifest.generated_at = "2026-05-30T13:00:00Z".to_string();
        let remote_url =
            spawn_rules_server(serde_json::to_vec(&manifest).expect("serialize remote manifest"))
                .await;
        let fixture =
            TestFixture::new_with_remote_url("rules-refresh-configured", Some(remote_url));

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri("/api/v1/performance/rules/refresh")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let status: PerformanceRulesStatus =
            serde_json::from_slice(&body).expect("rules status json");
        assert_eq!(status.rule_source, croopor_performance::RuleSource::Remote);
        assert_eq!(
            status.rule_channel,
            croopor_performance::RuleChannel::Remote
        );
        assert!(status.remote_refresh);
        assert_eq!(status.generated_at, manifest.generated_at);
        assert_eq!(
            status.validation,
            croopor_performance::RulesValidation::Valid
        );
        assert!(status.warnings.is_empty());
    }

    #[tokio::test]
    async fn plan_missing_game_version_returns_json_error() {
        let fixture = TestFixture::new("plan-missing-game-version");

        let error = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: None,
                loader: None,
                mode: None,
            }),
        )
        .await
        .expect_err("missing game_version should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "game_version query parameter is required" })
        );
    }

    #[tokio::test]
    async fn plan_invalid_mode_returns_json_error() {
        let fixture = TestFixture::new("plan-invalid-mode");

        let error = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.20.4".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some("turbo".to_string()),
            }),
        )
        .await
        .expect_err("invalid mode should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid performance mode: turbo" })
        );
    }

    #[tokio::test]
    async fn plan_custom_mode_serializes_as_inactive() {
        let fixture = TestFixture::new("plan-custom-mode");

        let Json(response) = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some(" 1.20.4 ".to_string()),
                loader: Some(" fabric ".to_string()),
                mode: Some("custom".to_string()),
            }),
        )
        .await
        .expect("custom plan should serialize");

        assert!(!response.active);
        assert_eq!(response.plan.mode, PerformanceMode::Custom);
        assert_eq!(response.plan.loader, "fabric");
        assert!(response.plan.mods.is_empty());
    }

    #[tokio::test]
    async fn health_custom_mode_ignores_corrupt_state_and_has_one_warnings_field() {
        let fixture = TestFixture::new("health-custom-corrupt-state");
        let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
        let mut instance = fixture
            .state
            .instances()
            .get(&instance_id)
            .expect("instance should exist");
        instance.performance_mode = "custom".to_string();
        fixture
            .state
            .instances()
            .update(instance)
            .expect("update instance");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::write(mods_dir.join(".croopor-lock.json"), "{not json").expect("write corrupt state");

        let Json(response) = handle_health(
            State(fixture.state.clone()),
            Query(HealthQuery {
                instance_id: Some(instance_id),
            }),
        )
        .await
        .expect("custom health should not read state");

        assert!(!response.active);
        assert_eq!(response.health, BundleHealth::Disabled);
        assert!(response.warnings.is_empty());
        let value = serde_json::to_value(&response).expect("serialize response");
        let object = value.as_object().expect("response object");
        assert_eq!(
            object
                .keys()
                .filter(|key| key.as_str() == "warnings")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn health_response_includes_bounded_managed_artifact_summary() {
        let fixture = TestFixture::new("health-managed-artifacts");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed file");
        croopor_performance::state::save_state(
            &mods_dir,
            &test_composition_state(
                "core",
                vec![InstalledMod {
                    project_id: "sodium".to_string(),
                    version_id: "version-a".to_string(),
                    filename: "managed.jar".to_string(),
                    ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
                    sha512: "abc123".to_string(),
                }],
            ),
        )
        .expect("save state");

        let Json(response) = handle_health(
            State(fixture.state.clone()),
            Query(HealthQuery {
                instance_id: Some(instance_id),
            }),
        )
        .await
        .expect("managed health should serialize");

        assert!(response.active);
        assert_eq!(response.installed_count, 1);
        assert_eq!(
            response.managed_artifacts,
            vec![PerformanceManagedArtifactSummary {
                project_id: "sodium".to_string(),
                version_id: "version-a".to_string(),
                filename: "managed.jar".to_string(),
                ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
                sha512_present: true,
            }]
        );
        let value = serde_json::to_value(&response).expect("serialize response");
        assert!(value.get("managed_artifacts").is_some());
        assert!(value.to_string().contains("managed.jar"));
        assert!(!value.to_string().contains(&mods_dir.display().to_string()));
        assert!(!value.to_string().contains("abc123"));
    }

    #[tokio::test]
    async fn health_invalidates_user_managed_artifact_in_tracked_state() {
        let fixture = TestFixture::new("health-user-managed-state");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(
            mods_dir.join(".croopor-lock.json"),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "user.jar",
                    "ownership_class": "user_managed",
                    "sha512": ""
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write state");

        let Json(response) = handle_health(
            State(fixture.state.clone()),
            Query(HealthQuery {
                instance_id: Some(instance_id),
            }),
        )
        .await
        .expect("invalid ownership should become health response");

        assert_eq!(response.health, BundleHealth::Invalid);
        assert!(response.managed_artifacts.is_empty());
        assert_eq!(
            response.warnings,
            vec!["invalid performance artifact ownership metadata".to_string()]
        );
    }

    #[tokio::test]
    async fn install_missing_instance_id_returns_json_error() {
        let fixture = TestFixture::new("install-missing-instance-id");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: None,
                game_version: None,
                loader: None,
                mode: None,
                action: None,
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("missing instance_id should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance_id is required" })
        );
    }

    #[tokio::test]
    async fn install_missing_instance_returns_json_error() {
        let fixture = TestFixture::new("install-missing-instance");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some("missing".to_string()),
                game_version: None,
                loader: None,
                mode: None,
                action: None,
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("missing instance should fail");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "instance not found" })
        );
    }

    #[tokio::test]
    async fn install_custom_mode_removes_only_managed_artifacts() {
        let fixture = TestFixture::new("install-custom-remove");
        let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed mod");
        fs::write(mods_dir.join("user.jar"), b"user").expect("write user mod");
        fs::write(
            mods_dir.join(".croopor-lock.json"),
            serde_json::to_vec(&croopor_performance::CompositionState {
                composition_id: "core".to_string(),
                tier: CompositionTier::Core,
                installed_mods: vec![croopor_performance::InstalledMod {
                    project_id: "sodium".to_string(),
                    version_id: "version".to_string(),
                    filename: "managed.jar".to_string(),
                    ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
                    sha512: String::new(),
                }],
                installed_at: "2026-05-30T00:00:00Z".to_string(),
                failure_count: 0,
                last_failure: String::new(),
            })
            .expect("serialize state"),
        )
        .expect("write state");

        let Json(response) = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: Some("custom".to_string()),
                action: None,
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect("custom mode should remove managed bundle");

        assert!(!response.active);
        assert_eq!(response.status, "removed");
        assert_eq!(response.health, BundleHealth::Disabled);
        assert_eq!(response.installed_count, 0);
        assert!(response.warnings.is_empty());
        assert!(!mods_dir.join("managed.jar").exists());
        assert!(!mods_dir.join(".croopor-lock.json").exists());
        assert!(mods_dir.join("user.jar").is_file());
    }

    #[tokio::test]
    async fn install_remove_rejects_invalid_ownership_without_deleting_files() {
        let fixture = TestFixture::new("install-invalid-ownership-remove");
        let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("user.jar"), b"user").expect("write user file");
        fs::write(
            mods_dir.join(".croopor-lock.json"),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "user.jar",
                    "ownership_class": "user_managed",
                    "sha512": ""
                }],
                "installed_at": "2026-05-30T00:00:00Z"
            }))
            .expect("serialize state"),
        )
        .expect("write invalid state");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: Some("custom".to_string()),
                action: None,
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("invalid ownership should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "invalid performance artifact ownership metadata"
            })
        );
        assert_eq!(
            fs::read(mods_dir.join("user.jar")).expect("read user"),
            b"user"
        );
        assert!(mods_dir.join(".croopor-lock.json").is_file());
    }

    #[tokio::test]
    async fn rollback_without_snapshot_returns_json_error() {
        let fixture = TestFixture::new("rollback-missing");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("rollback".to_string()),
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("missing rollback snapshot should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "no performance rollback snapshot available" })
        );
    }

    #[tokio::test]
    async fn rollback_list_route_returns_snapshot_metadata() {
        let fixture = TestFixture::new("rollback-list");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("managed-a.jar"), b"managed-a").expect("write managed a");
        fs::write(mods_dir.join("managed-b.jar"), b"managed-b").expect("write managed b");
        let first = croopor_performance::state::save_rollback_snapshot(
            &mods_dir,
            &test_composition_state(
                "core-a",
                vec![test_installed_mod("sodium", "managed-a.jar")],
            ),
        )
        .expect("save first snapshot");
        let second = croopor_performance::state::save_rollback_snapshot(
            &mods_dir,
            &test_composition_state(
                "core-b",
                vec![test_installed_mod("lithium", "managed-b.jar")],
            ),
        )
        .expect("save second snapshot");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/performance/rollback?instance_id={instance_id}"
                    ))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("rollback list json");
        let snapshots = value["snapshots"].as_array().expect("snapshots array");

        assert_eq!(snapshots.len(), 2);
        assert!(snapshots.iter().any(|snapshot| {
            snapshot["id"] == first.id
                && snapshot["composition_id"] == "core-a"
                && snapshot["latest"] == false
        }));
        assert!(snapshots.iter().any(|snapshot| {
            snapshot["id"] == second.id
                && snapshot["composition_id"] == "core-b"
                && snapshot["latest"] == true
        }));
    }

    #[tokio::test]
    async fn rollback_with_specific_snapshot_id_restores_older_snapshot() {
        let fixture = TestFixture::new("rollback-specific");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("managed-a.jar"), b"managed-a").expect("write managed a");
        let older = croopor_performance::state::save_rollback_snapshot(
            &mods_dir,
            &test_composition_state(
                "core-a",
                vec![test_installed_mod("sodium", "managed-a.jar")],
            ),
        )
        .expect("save older snapshot");
        fs::write(mods_dir.join("managed-b.jar"), b"managed-b").expect("write managed b");
        croopor_performance::state::save_state(
            &mods_dir,
            &test_composition_state(
                "core-b",
                vec![test_installed_mod("lithium", "managed-b.jar")],
            ),
        )
        .expect("save current state");
        croopor_performance::state::save_rollback_snapshot(
            &mods_dir,
            &test_composition_state(
                "core-b",
                vec![test_installed_mod("lithium", "managed-b.jar")],
            ),
        )
        .expect("save newer snapshot");

        let Json(response) = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("rollback".to_string()),
                rollback_id: Some(older.id.clone()),
                queued: None,
            }),
        )
        .await
        .expect("specific rollback should restore");

        assert_eq!(response.status, "rolled_back");
        assert_eq!(response.composition_id, "core-a");
        assert_eq!(
            response.managed_artifacts,
            vec![PerformanceManagedArtifactSummary {
                project_id: "sodium".to_string(),
                version_id: "version".to_string(),
                filename: "managed-a.jar".to_string(),
                ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
                sha512_present: false,
            }]
        );
        assert_eq!(
            fs::read(mods_dir.join("managed-a.jar")).expect("read managed a"),
            b"managed-a"
        );
        assert!(!mods_dir.join("managed-b.jar").exists());
    }

    #[tokio::test]
    async fn rollback_invalid_snapshot_id_returns_json_error() {
        let fixture = TestFixture::new("rollback-invalid-id");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("rollback".to_string()),
                rollback_id: Some("../latest".to_string()),
                queued: None,
            }),
        )
        .await
        .expect_err("invalid rollback id should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid performance rollback snapshot id" })
        );
    }

    #[tokio::test]
    async fn rollback_missing_snapshot_id_returns_json_error() {
        let fixture = TestFixture::new("rollback-missing-id");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("rollback".to_string()),
                rollback_id: Some("rb-missing".to_string()),
                queued: None,
            }),
        )
        .await
        .expect_err("missing rollback id should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "performance rollback snapshot not found" })
        );
    }

    #[tokio::test]
    async fn queued_remove_returns_install_id_and_complete_progress() {
        let fixture = TestFixture::new("queued-remove");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let Json(response) = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id.clone()),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(true),
            }),
        )
        .await
        .expect("queued remove should be accepted");

        assert_eq!(response.status, "queued");
        let install_id = response.install_id.expect("queued response has install id");
        let events = collect_install_events(&fixture.state, &install_id).await;
        let phases = events
            .iter()
            .map(|event| event.phase.as_str())
            .collect::<Vec<_>>();
        assert_eq!(phases, vec!["queued", "planning", "removing", "complete"]);
        let terminal = events.last().expect("terminal event");
        assert!(terminal.done);
        assert!(terminal.error.is_none());
        let status = fixture
            .state
            .performance_operations()
            .get(&install_id)
            .await
            .expect("durable operation status");
        assert_eq!(status.instance_id, instance_id);
        assert_eq!(status.action, "remove");
        assert_eq!(status.state, "complete");
        assert_eq!(status.error, None);
    }

    #[tokio::test]
    async fn queued_rollback_without_snapshot_emits_terminal_error() {
        let fixture = TestFixture::new("queued-rollback-missing");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let Json(response) = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("rollback".to_string()),
                rollback_id: None,
                queued: Some(true),
            }),
        )
        .await
        .expect("queued rollback should be accepted");

        assert_eq!(response.status, "queued");
        let install_id = response.install_id.expect("queued response has install id");
        let events = collect_install_events(&fixture.state, &install_id).await;
        let terminal = events.last().expect("terminal event");
        assert_eq!(terminal.phase, "error");
        assert!(terminal.done);
        assert_eq!(
            terminal.error.as_deref(),
            Some("no performance rollback snapshot available")
        );
        let status = fixture
            .state
            .performance_operations()
            .get(&install_id)
            .await
            .expect("durable operation status");
        assert_eq!(status.action, "rollback");
        assert_eq!(status.state, "failed");
        assert_eq!(
            status.error.as_deref(),
            Some("no performance rollback snapshot available")
        );
    }

    #[tokio::test]
    async fn queued_operation_rejects_same_instance_overlap() {
        let fixture = TestFixture::new("queued-overlap");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        fixture
            .state
            .performance_operations()
            .start(instance_id.clone(), "remove".to_string())
            .await
            .expect("prelock instance");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("remove".to_string()),
                rollback_id: None,
                queued: Some(true),
            }),
        )
        .await
        .expect_err("overlapping queued operation should fail");

        assert_eq!(error.0, StatusCode::CONFLICT);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "a performance operation is already queued for this instance"
            })
        );
    }

    #[tokio::test]
    async fn operation_status_route_returns_persisted_status() {
        let fixture = TestFixture::new("operation-status-route");
        let started = fixture
            .state
            .performance_operations()
            .start("instance-a".to_string(), "install".to_string())
            .await
            .expect("operation starts");
        fixture
            .state
            .performance_operations()
            .record_progress(&started.id, "applying")
            .await;

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!("/api/v1/performance/operations/{}", started.id))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");
        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let response: PerformanceOperationStatus =
            serde_json::from_slice(&body).expect("operation status json");

        assert_eq!(response.id, started.id);
        assert_eq!(response.instance_id, "instance-a");
        assert_eq!(response.action, "install");
        assert_eq!(response.state, "applying");
    }

    #[tokio::test]
    async fn missing_operation_status_route_returns_json_error() {
        let fixture = TestFixture::new("operation-status-missing");

        let error = handle_operation_status(
            State(fixture.state.clone()),
            Path("performance-install-00000000000000000000000000000000".to_string()),
        )
        .await
        .expect_err("missing operation should fail");

        assert_eq!(error.0, StatusCode::NOT_FOUND);
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "performance operation not found" })
        );
    }

    async fn collect_install_events(state: &AppState, install_id: &str) -> Vec<DownloadProgress> {
        let (mut events, mut receiver, done) = state
            .installs()
            .subscribe(install_id)
            .await
            .expect("install session should exist");
        if done || events.iter().any(|event| event.done) {
            return events;
        }

        loop {
            let event = tokio::time::timeout(Duration::from_secs(2), receiver.recv())
                .await
                .expect("progress event should arrive")
                .expect("progress receiver should stay open");
            let terminal = event.done;
            events.push(event);
            if terminal {
                return events;
            }
        }
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            Self::new_with_remote_url(name, None)
        }

        fn new_with_remote_url(name: &str, remote_rules_url: Option<String>) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances =
                Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
            let state = AppState::new(AppStateInit {
                app_name: "Croopor".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::new_with_config_dir_and_remote_url(
                        &paths.config_dir,
                        remote_rules_url,
                    )
                    .expect("performance manager"),
                ),
                frontend_dir: root.join("frontend"),
            });

            Self { state, root }
        }

        fn add_instance(&self, name: &str, version_id: &str) -> String {
            self.state
                .instances()
                .add(
                    name.to_string(),
                    version_id.to_string(),
                    String::new(),
                    String::new(),
                    None,
                )
                .expect("add instance")
                .id
        }
    }

    async fn spawn_rules_server(body: Vec<u8>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind rules server");
        let addr = listener.local_addr().expect("rules server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept rules request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket
                .write_all(header.as_bytes())
                .await
                .expect("write rules response header");
            socket
                .write_all(&body)
                .await
                .expect("write rules response body");
        });
        format!("http://{addr}/rules.json")
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-performance-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &std::path::Path) -> AppPaths {
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
    }

    fn test_composition_state(
        composition_id: &str,
        installed_mods: Vec<InstalledMod>,
    ) -> CompositionState {
        CompositionState {
            composition_id: composition_id.to_string(),
            tier: CompositionTier::Core,
            installed_mods,
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        }
    }

    fn test_installed_mod(project_id: &str, filename: &str) -> InstalledMod {
        InstalledMod {
            project_id: project_id.to_string(),
            version_id: "version".to_string(),
            filename: filename.to_string(),
            ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
            sha512: String::new(),
        }
    }
}
