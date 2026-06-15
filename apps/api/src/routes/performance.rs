use crate::application::{self, RefreshPerformanceRulesError};
use crate::guardian::{
    GuardianFact, performance_failure_memory_guardian_fact, performance_health_guardian_facts,
    performance_plan_guardian_facts, performance_state_error_guardian_fact,
};
use crate::observability::{
    PerformanceProofRecord, RedactionAudience, performance_health_proof_record,
    sanitize_evidence_token,
};
use crate::state::contracts::{
    CommandKind, JournalId, OperationId, OperationJournalEntry, OperationJournalStep,
    OperationOutcome, OperationPhase, OperationStepResult, OwnershipClass as StateOwnershipClass,
    RollbackState, StabilizationSystem, TargetDescriptor, TargetKind,
};
use crate::state::performance_operations::{
    PerformanceOperationConflict, PerformanceOperationPayload, PerformanceOperationStatus,
    sanitize_operation_error,
};
use crate::state::{AppState, DownloadProgress};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_minecraft::scan_versions;
use croopor_performance::InstallError;
use croopor_performance::{
    BundleHealth, CompositionPlan, CompositionState, CompositionTier, ManagedArtifactProvider,
    OwnershipClass, PerformanceMode, ResolutionRequest, RollbackSnapshotSummary, StateError,
    derive_health, effective_performance_plan, load_state, parse_mode,
};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

const PERFORMANCE_MANAGED_ARTIFACT_SUMMARY_LIMIT: usize = 50;
const PERFORMANCE_GUARDIAN_FACT_LIMIT: usize = 16;
const INVALID_PERSISTED_OPERATION_ERROR: &str = "invalid persisted performance operation payload";
const PERFORMANCE_DATA_INTERNAL_ERROR: &str =
    "Could not load performance data. Check app data permissions and try again.";
const PERFORMANCE_INSTALL_INTERNAL_ERROR: &str =
    "Could not update managed performance files. Check instance folder permissions and try again.";
const PERFORMANCE_STATE_PARSE_WARNING: &str = "failed to parse performance state";

#[derive(Debug, Deserialize)]
struct PlanQuery {
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    instance_id: Option<String>,
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
    effective: croopor_performance::EffectivePerformancePlan,
    guardian_facts: Vec<GuardianFact>,
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
    guardian_facts: Vec<GuardianFact>,
    proof: PerformanceProofRecord,
    view_model: application::PerformancePlanSummaryViewModel,
    display: PerformanceInstanceDisplay,
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

#[derive(Debug, Serialize)]
struct PerformanceInstanceOperationResponse {
    operation: Option<PerformanceOperationStatus>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
struct PerformanceManagedArtifactSummary {
    project_id: String,
    version_id: String,
    filename: String,
    ownership_class: OwnershipClass,
    source_provider: ManagedArtifactProvider,
    sha512_present: bool,
    sha512_verified: bool,
}

#[derive(Debug, Clone, Serialize)]
struct PerformanceInstanceDisplay {
    memory: PerformanceMemoryDisplay,
    runtime: PerformanceRuntimeDisplay,
    mode: PerformanceModeDisplay,
}

#[derive(Debug, Clone, Serialize)]
struct PerformanceMemoryDisplay {
    min_gb: f32,
    max_gb: f32,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct PerformanceRuntimeDisplay {
    detected: bool,
    label: String,
}

#[derive(Debug, Clone, Serialize)]
struct PerformanceModeDisplay {
    mode: String,
    label: String,
    source: String,
    source_label: String,
}

#[derive(Debug, Serialize)]
struct PerformanceRollbackListResponse {
    snapshots: Vec<RollbackSnapshotSummary>,
}

#[derive(Debug, Clone)]
struct PerformanceOperation {
    instance_id: String,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: PerformanceInstallAction,
    rollback_id: Option<String>,
}

pub(crate) fn spawn_pending_performance_operations(state: &AppState) -> bool {
    let state = state.clone();
    tokio::spawn(async move {
        let resumed = resume_pending_performance_operations(state).await;
        if resumed > 0 {
            tracing::info!(
                resumed,
                "queued performance operations resumed after restart"
            );
        }
    });
    true
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
            "/api/v1/performance/instances/{instance_id}/operation",
            get(handle_instance_operation),
        )
        .route(
            "/api/v1/performance/operations/{id}",
            get(handle_operation_status),
        )
}

async fn handle_status(
    State(state): State<AppState>,
) -> Result<Json<application::PerformanceRulesStatusResponse>, (StatusCode, Json<serde_json::Value>)>
{
    Ok(Json(application::performance_rules_status(&state)))
}

async fn handle_rules_refresh(
    State(state): State<AppState>,
) -> Result<Json<application::PerformanceRulesStatusResponse>, (StatusCode, Json<serde_json::Value>)>
{
    match application::refresh_performance_rules(&state).await {
        Ok(response) => Ok(Json(response)),
        Err(RefreshPerformanceRulesError::Unconfigured) => Err((
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
    let installed_mods = plan_installed_mod_evidence(&state, query.instance_id.as_deref())?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader: optional_value(query.loader.as_deref()).unwrap_or_default(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods,
    });

    let mut guardian_facts = performance_plan_guardian_facts(&plan, OperationPhase::Planning);
    append_performance_guardian_facts(
        &mut guardian_facts,
        performance_failure_memory_facts(
            &state,
            OperationPhase::Planning,
            Some(&plan.composition_id),
        ),
    );

    Ok(Json(PerformancePlanResponse {
        active: matches!(mode, PerformanceMode::Managed),
        effective: effective_performance_plan(&plan),
        guardian_facts,
        plan,
    }))
}

fn plan_installed_mod_evidence(
    state: &AppState,
    raw_instance_id: Option<&str>,
) -> Result<Vec<String>, (StatusCode, Json<serde_json::Value>)> {
    let Some(instance_id) = optional_value(raw_instance_id) else {
        return Ok(Vec::new());
    };
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = match load_state(&mods_dir) {
        Ok(state_file) => state_file,
        Err(StateError::Parse(_)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "failed to parse performance state" })),
            ));
        }
        Err(StateError::InvalidOwnership { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid performance artifact ownership metadata"
                })),
            ));
        }
        Err(StateError::InvalidIntegrity { .. }) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "invalid performance artifact integrity metadata"
                })),
            ));
        }
        Err(error) => return Err(internal_error(error)),
    };

    Ok(installed_mod_evidence(&mods_dir, state_file.as_ref()))
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
    let mode = resolve_instance_mode(&state, &instance, None)?;
    let display = performance_instance_display(&state, &instance, mode);

    if !matches!(mode, PerformanceMode::Managed) {
        return Ok(Json(disabled_health_response(mode, display)));
    }

    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = match load_state(&mods_dir) {
        Ok(state_file) => state_file,
        Err(StateError::Parse(_)) => {
            return Ok(Json(invalid_health_response(
                PERFORMANCE_STATE_PARSE_WARNING,
                Vec::new(),
                display,
            )));
        }
        Err(error @ StateError::InvalidOwnership { .. }) => {
            return Ok(Json(invalid_health_response(
                "invalid performance artifact ownership metadata",
                performance_state_error_guardian_fact(&error, OperationPhase::Validating)
                    .into_iter()
                    .collect(),
                display,
            )));
        }
        Err(StateError::InvalidIntegrity { .. }) => {
            return Ok(Json(invalid_health_response(
                "invalid performance artifact integrity metadata",
                Vec::new(),
                display,
            )));
        }
        Err(error) => return Err(internal_error(error)),
    };
    let (game_version, loader) = resolve_instance_version_target(&state, &instance, None, None)?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version,
        loader,
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_evidence(&mods_dir, state_file.as_ref()),
    });
    let (health, warnings) = derive_health(state_file.as_ref(), Some(&plan), &mods_dir);
    let warnings = response_warnings(&plan, warnings);
    let composition_id = state_file
        .as_ref()
        .map(|value| value.composition_id.clone())
        .unwrap_or_default();
    let tier = state_file
        .as_ref()
        .map(|value| tier_name(value.tier).to_string())
        .unwrap_or_default();
    let installed_count = state_file
        .as_ref()
        .map(|value| value.installed_mods.len())
        .unwrap_or_default();
    let guardian_facts = performance_health_guardian_facts(
        health,
        &composition_id,
        &warnings,
        OperationPhase::Validating,
    );
    let mut guardian_facts = guardian_facts;
    append_performance_guardian_facts(
        &mut guardian_facts,
        performance_failure_memory_facts(&state, OperationPhase::Validating, Some(&composition_id)),
    );
    let proof = performance_health_proof(
        None,
        health,
        &composition_id,
        &tier,
        installed_count,
        warnings.len(),
        health_rollback_state(&state, &mods_dir),
    );
    let view_model = application::performance_plan_summary_view_model(
        mode,
        Some(&plan),
        health,
        state_file.as_ref().map(|value| value.tier),
        installed_count,
        &warnings,
    );

    Ok(Json(PerformanceHealthResponse {
        active: true,
        health,
        composition_id,
        tier,
        installed_count,
        managed_artifacts: managed_artifact_summary(state_file.as_ref()),
        warnings,
        guardian_facts,
        proof,
        view_model,
        display,
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
        .map(public_performance_operation_status)
        .map(Json)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "performance operation not found" })),
            )
        })
}

async fn handle_instance_operation(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> Result<Json<PerformanceInstanceOperationResponse>, (StatusCode, Json<serde_json::Value>)> {
    let instance = state.instances().get(&instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let operation = state
        .performance_operations()
        .current_or_latest_for_instance(&instance.id)
        .await
        .map(public_performance_operation_status);

    Ok(Json(PerformanceInstanceOperationResponse { operation }))
}

async fn execute_performance_operation(
    state: &AppState,
    operation: &PerformanceOperation,
) -> Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)> {
    let performance = state.performance().clone();
    let instance = state
        .instances()
        .get(&operation.instance_id)
        .ok_or_else(|| {
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": "instance not found" })),
            )
        })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");

    if matches!(operation.action, PerformanceInstallAction::Rollback) {
        let preflight = rollback_preflight(&mods_dir, operation.rollback_id.as_deref());
        let (target_id, rollback_state) = match &preflight {
            Ok((target_id, rollback_state)) => (target_id.clone(), *rollback_state),
            Err(_) => (
                "performance_rollback_snapshot".to_string(),
                RollbackState::Unavailable,
            ),
        };
        let operation_id = begin_performance_operation_journal(
            state,
            operation.action,
            &target_id,
            rollback_state,
        );
        if let Err(error) = preflight {
            record_performance_operation_failure(
                state,
                &operation_id,
                operation.action,
                &target_id,
                rollback_state,
            );
            return Err(error);
        }

        let result = (|| {
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

            Ok(PerformanceInstallResponse {
                active: true,
                status: "rolled_back".to_string(),
                install_id: None,
                health,
                composition_id: restored_state.composition_id.clone(),
                tier: tier_name(restored_state.tier).to_string(),
                installed_count: restored_state.installed_mods.len(),
                managed_artifacts: managed_artifact_summary(Some(&restored_state)),
                warnings,
            })
        })();
        record_performance_operation_result(
            state,
            &operation_id,
            operation.action,
            &target_id,
            rollback_state,
            &result,
        );

        return result;
    }

    let mode = resolve_instance_mode(state, &instance, operation.mode.as_deref())?;

    if matches!(operation.action, PerformanceInstallAction::Remove)
        || !matches!(mode, PerformanceMode::Managed)
    {
        let journal_action = PerformanceInstallAction::Remove;
        let current_state = preflight_current_performance_state(&mods_dir);
        let (target_id, rollback_state) = match &current_state {
            Ok(state) => (
                state
                    .as_ref()
                    .map(|state| state.composition_id.clone())
                    .unwrap_or_else(|| "performance_composition_lock".to_string()),
                rollback_state_for_current_state(state.as_ref()),
            ),
            Err(_) => (
                "performance_composition_lock".to_string(),
                RollbackState::Unavailable,
            ),
        };
        let operation_id =
            begin_performance_operation_journal(state, journal_action, &target_id, rollback_state);
        if let Err(error) = current_state {
            record_performance_operation_failure(
                state,
                &operation_id,
                journal_action,
                &target_id,
                rollback_state,
            );
            return Err(error);
        }

        let result = performance
            .remove_managed(&mods_dir)
            .map(|_| removed_install_response())
            .map_err(performance_install_error);
        record_performance_operation_result(
            state,
            &operation_id,
            journal_action,
            &target_id,
            rollback_state,
            &result,
        );

        return result;
    }

    let (game_version, loader) = resolve_instance_version_target(
        state,
        &instance,
        operation.game_version.as_deref(),
        operation.loader.as_deref(),
    )?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: game_version.clone(),
        loader: loader.clone(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_evidence_from_mods_dir(&mods_dir),
    });
    let current_state = preflight_current_performance_state(&mods_dir);
    let rollback_state = match &current_state {
        Ok(state) => rollback_state_for_current_state(state.as_ref()),
        Err(_) => RollbackState::Unavailable,
    };
    let operation_id = begin_performance_operation_journal(
        state,
        operation.action,
        &plan.composition_id,
        rollback_state,
    );
    if let Err(error) = current_state {
        record_performance_operation_failure(
            state,
            &operation_id,
            operation.action,
            &plan.composition_id,
            rollback_state,
        );
        return Err(error);
    }

    let result = match performance
        .ensure_installed(&plan, &game_version, &mods_dir)
        .await
    {
        Ok(installed_state) => {
            let (health, warnings) = derive_health(Some(&installed_state), Some(&plan), &mods_dir);
            let warnings = response_warnings(&plan, warnings);
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
        Err(error) => Err(performance_install_error(error)),
    };
    record_performance_operation_result(
        state,
        &operation_id,
        operation.action,
        &plan.composition_id,
        rollback_state,
        &result,
    );

    result
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
            operation_payload(&operation),
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

async fn resume_pending_performance_operations(state: AppState) -> usize {
    let pending = state
        .performance_operations()
        .take_pending_resumable_operations()
        .await;
    let resumed = pending.len();

    for status in pending {
        let install_id = status.id.clone();
        let operation = match operation_from_status(&status) {
            Ok(operation) => operation,
            Err(error) => {
                state
                    .performance_operations()
                    .record_failed(&install_id, &error)
                    .await;
                continue;
            }
        };
        state.installs().insert(install_id.clone()).await;
        let store = state.installs().clone();
        let state_task = state.clone();
        tokio::spawn(async move {
            run_queued_performance_operation(state_task, operation, store, install_id).await;
        });
    }

    resumed
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

fn operation_payload(operation: &PerformanceOperation) -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: operation.game_version.clone(),
        loader: operation.loader.clone(),
        mode: operation.mode.clone(),
        rollback_id: operation.rollback_id.clone(),
    }
}

fn public_performance_operation_status(
    mut status: PerformanceOperationStatus,
) -> PerformanceOperationStatus {
    status.instance_id = public_operation_required_token(&status.instance_id, "redacted");
    status.action = public_operation_required_token(&status.action, "unknown");
    status.state = public_operation_required_token(&status.state, "unknown");
    status.payload = public_operation_payload(status.payload);
    status.error = status
        .error
        .as_deref()
        .map(sanitize_operation_error)
        .filter(|value| !value.trim().is_empty());
    status
}

fn public_operation_required_token(value: &str, fallback: &str) -> String {
    sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
        .unwrap_or_else(|| fallback.to_string())
}

fn public_operation_payload(payload: PerformanceOperationPayload) -> PerformanceOperationPayload {
    PerformanceOperationPayload {
        game_version: public_operation_payload_token(payload.game_version),
        loader: public_operation_payload_token(payload.loader),
        mode: public_operation_payload_token(payload.mode),
        rollback_id: public_operation_payload_token(payload.rollback_id),
    }
}

fn public_operation_payload_token(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let value = value.trim();
        if value.is_empty() {
            None
        } else {
            sanitize_evidence_token(value, RedactionAudience::UserVisible, 96)
                .or_else(|| Some("redacted".to_string()))
        }
    })
}

fn operation_from_status(
    status: &PerformanceOperationStatus,
) -> Result<PerformanceOperation, String> {
    let action = operation_action_from_name(&status.action)
        .ok_or_else(|| INVALID_PERSISTED_OPERATION_ERROR.to_string())?;
    if status.instance_id.trim().is_empty() {
        return Err(INVALID_PERSISTED_OPERATION_ERROR.to_string());
    }

    Ok(PerformanceOperation {
        instance_id: status.instance_id.clone(),
        game_version: status.payload.game_version.clone(),
        loader: status.payload.loader.clone(),
        mode: status.payload.mode.clone(),
        action,
        rollback_id: status.payload.rollback_id.clone(),
    })
}

fn operation_action_from_name(action: &str) -> Option<PerformanceInstallAction> {
    match action {
        "install" => Some(PerformanceInstallAction::Install),
        "remove" => Some(PerformanceInstallAction::Remove),
        "rollback" => Some(PerformanceInstallAction::Rollback),
        _ => None,
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
                    source_provider: installed.source.provider,
                    sha512_present: !installed.integrity.sha512.trim().is_empty(),
                    sha512_verified: installed.integrity.sha512_verified,
                })
                .collect()
        })
        .unwrap_or_default()
}

fn performance_failure_memory_facts(
    state: &AppState,
    phase: OperationPhase,
    target_id: Option<&str>,
) -> Vec<GuardianFact> {
    let target_id = target_id.map(str::trim).filter(|value| !value.is_empty());
    state
        .failure_memory()
        .list()
        .into_iter()
        .filter(|entry| match target_id {
            Some(target_id) => entry.target.id == target_id,
            None => true,
        })
        .filter_map(|entry| performance_failure_memory_guardian_fact(&entry, phase))
        .take(PERFORMANCE_GUARDIAN_FACT_LIMIT)
        .collect()
}

fn append_performance_guardian_facts(facts: &mut Vec<GuardianFact>, more: Vec<GuardianFact>) {
    let remaining = PERFORMANCE_GUARDIAN_FACT_LIMIT.saturating_sub(facts.len());
    facts.extend(more.into_iter().take(remaining));
    facts.truncate(PERFORMANCE_GUARDIAN_FACT_LIMIT);
}

fn health_rollback_state(state: &AppState, mods_dir: &std::path::Path) -> RollbackState {
    match state.performance().list_rollback_snapshots(mods_dir) {
        Ok(snapshots) if !snapshots.is_empty() => RollbackState::Available,
        _ => RollbackState::Unavailable,
    }
}

fn performance_health_proof(
    operation_id: Option<OperationId>,
    health: BundleHealth,
    composition_id: &str,
    tier: &str,
    installed_count: usize,
    warning_count: usize,
    rollback: RollbackState,
) -> PerformanceProofRecord {
    performance_health_proof_record(
        operation_id,
        performance_composition_target(composition_id),
        bundle_health_token(health),
        rollback,
        vec![
            ("composition_id", proof_token(composition_id, "none")),
            ("tier", proof_token(tier, "none")),
            ("managed_artifact_count", installed_count.to_string()),
            ("warning_count", warning_count.to_string()),
        ],
    )
}

fn bundle_health_token(health: BundleHealth) -> &'static str {
    match health {
        BundleHealth::Healthy => "healthy",
        BundleHealth::Degraded => "degraded",
        BundleHealth::Fallback => "fallback",
        BundleHealth::Disabled => "disabled",
        BundleHealth::Invalid => "invalid",
    }
}

fn proof_token(value: &str, fallback: &str) -> String {
    let value = value.trim();
    if value.is_empty() {
        fallback.to_string()
    } else {
        value.to_string()
    }
}

fn performance_composition_target(composition_id: &str) -> TargetDescriptor {
    let id = composition_id.trim();
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::PerformanceComposition,
        if id.is_empty() {
            "performance_composition"
        } else {
            id
        },
        StateOwnershipClass::CompositionManaged,
    )
}

fn performance_artifacts_target(composition_id: &str) -> TargetDescriptor {
    let id = composition_id.trim();
    let target_id = if id.is_empty() {
        "managed_performance_artifacts".to_string()
    } else {
        format!("{id}_managed_artifacts")
    };
    TargetDescriptor::new(
        StabilizationSystem::Performance,
        TargetKind::Artifact,
        target_id,
        StateOwnershipClass::CompositionManaged,
    )
}

fn preflight_current_performance_state(
    mods_dir: &std::path::Path,
) -> Result<Option<CompositionState>, (StatusCode, Json<serde_json::Value>)> {
    load_state(mods_dir).map_err(|error| performance_install_error(InstallError::State(error)))
}

fn rollback_preflight(
    mods_dir: &std::path::Path,
    rollback_id: Option<&str>,
) -> Result<(String, RollbackState), (StatusCode, Json<serde_json::Value>)> {
    let snapshot = if let Some(rollback_id) = optional_value(rollback_id) {
        croopor_performance::state::load_rollback_snapshot_by_id(mods_dir, &rollback_id)
            .map_err(|error| performance_install_error(InstallError::State(error)))?
    } else {
        croopor_performance::state::load_rollback_snapshot(mods_dir)
            .map_err(|error| performance_install_error(InstallError::State(error)))?
    };

    Ok(match snapshot {
        Some(snapshot) => (snapshot.state.composition_id, RollbackState::Available),
        None => (
            "performance_rollback_snapshot".to_string(),
            RollbackState::Unavailable,
        ),
    })
}

fn rollback_state_for_current_state(state: Option<&CompositionState>) -> RollbackState {
    if state.is_some() {
        RollbackState::Available
    } else {
        RollbackState::Unavailable
    }
}

fn begin_performance_operation_journal(
    state: &AppState,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) -> OperationId {
    let operation_id = OperationId::new(format!(
        "performance-{}-{}",
        performance_operation_step_id(action),
        uuid::Uuid::new_v4()
    ));
    let mut entry = OperationJournalEntry::new(
        JournalId::new(format!("journal-{}", operation_id.as_str())),
        operation_id.clone(),
        CommandKind::ApplyPerformancePlan,
        StabilizationSystem::Application,
        StateOwnershipClass::CompositionManaged,
        rollback,
    );
    entry.targets = performance_operation_targets(target_id);
    entry.planned_steps.push(performance_operation_journal_step(
        action,
        OperationStepResult::Planned,
        target_id,
        rollback,
    ));
    state.journals().create(entry);
    operation_id
}

fn record_performance_operation_result(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    fallback_target_id: &str,
    rollback: RollbackState,
    result: &Result<PerformanceInstallResponse, (StatusCode, Json<serde_json::Value>)>,
) {
    match result {
        Ok(response) => {
            let response_target_id = response.composition_id.trim();
            let target_id = if response_target_id.is_empty() {
                fallback_target_id
            } else {
                response_target_id
            };
            record_performance_operation_success(state, operation_id, action, target_id, rollback);
        }
        Err(_) => record_performance_operation_failure(
            state,
            operation_id,
            action,
            fallback_target_id,
            rollback,
        ),
    }
}

fn record_performance_operation_success(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) {
    let rollback = if matches!(action, PerformanceInstallAction::Rollback) {
        RollbackState::Applied
    } else {
        rollback
    };
    state.journals().record_success(
        operation_id,
        performance_operation_journal_step(
            action,
            OperationStepResult::Completed,
            target_id,
            rollback,
        ),
        OperationOutcome::Succeeded,
    );
}

fn record_performance_operation_failure(
    state: &AppState,
    operation_id: &OperationId,
    action: PerformanceInstallAction,
    target_id: &str,
    rollback: RollbackState,
) {
    state.journals().record_failure(
        operation_id,
        performance_operation_journal_step(
            action,
            OperationStepResult::Failed,
            target_id,
            rollback,
        ),
        performance_operation_step_id(action),
        OperationOutcome::Failed,
    );
}

fn performance_operation_journal_step(
    action: PerformanceInstallAction,
    result: OperationStepResult,
    target_id: &str,
    rollback: RollbackState,
) -> OperationJournalStep {
    let mut step = OperationJournalStep::new(
        performance_operation_step_id(action),
        performance_operation_phase(action),
    );
    step.result = result;
    step.rollback = rollback;
    step.generated_facts
        .push("performance_operation_evidence".to_string());
    if !matches!(rollback, RollbackState::NotApplicable) {
        step.generated_facts
            .push("performance_rollback_evidence".to_string());
    }
    if result != OperationStepResult::Planned {
        step.changed_target = Some(performance_composition_target(target_id));
    }
    step
}

fn performance_operation_targets(target_id: &str) -> Vec<TargetDescriptor> {
    vec![
        performance_composition_target(target_id),
        performance_artifacts_target(target_id),
    ]
}

fn performance_operation_step_id(action: PerformanceInstallAction) -> &'static str {
    match action {
        PerformanceInstallAction::Install => "apply_performance_plan",
        PerformanceInstallAction::Remove => "remove_performance_plan",
        PerformanceInstallAction::Rollback => "rollback_performance_plan",
    }
}

fn performance_operation_phase(action: PerformanceInstallAction) -> OperationPhase {
    match action {
        PerformanceInstallAction::Install | PerformanceInstallAction::Remove => {
            OperationPhase::Installing
        }
        PerformanceInstallAction::Rollback => OperationPhase::RollingBack,
    }
}

fn performance_instance_display(
    state: &AppState,
    instance: &croopor_config::Instance,
    mode: PerformanceMode,
) -> PerformanceInstanceDisplay {
    let config = state.config().current();
    let min_gb = memory_gb(instance.min_memory_mb, config.min_memory_mb, 1024);
    let max_gb = memory_gb(instance.max_memory_mb, config.max_memory_mb, 4096);
    let java_major = instance_java_major(state, &instance.version_id);
    let mode_source = if parse_mode(&instance.performance_mode).is_some() {
        ("instance", "Per instance")
    } else {
        ("global", "Global default")
    };

    PerformanceInstanceDisplay {
        memory: PerformanceMemoryDisplay {
            min_gb,
            max_gb,
            label: heap_label(min_gb, max_gb),
        },
        runtime: PerformanceRuntimeDisplay {
            detected: java_major.is_some(),
            label: java_major
                .map(|major| format!("Java {major}"))
                .unwrap_or_else(|| "Managed Java".to_string()),
        },
        mode: PerformanceModeDisplay {
            mode: performance_mode_token(mode).to_string(),
            label: performance_mode_label(mode).to_string(),
            source: mode_source.0.to_string(),
            source_label: mode_source.1.to_string(),
        },
    }
}

fn instance_java_major(state: &AppState, version_id: &str) -> Option<i32> {
    state
        .library_dir()
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .and_then(|path| scan_versions(&path).ok())
        .and_then(|versions| {
            versions
                .into_iter()
                .find(|version| version.id == version_id)
                .and_then(|version| (version.java_major > 0).then_some(version.java_major))
        })
}

fn memory_gb(instance_mb: i32, config_mb: i32, fallback_mb: i32) -> f32 {
    let mb = if instance_mb > 0 {
        instance_mb
    } else if config_mb > 0 {
        config_mb
    } else {
        fallback_mb
    };
    mb as f32 / 1024.0
}

fn heap_label(min_gb: f32, max_gb: f32) -> String {
    if (min_gb - max_gb).abs() < f32::EPSILON {
        format!("{} GB", fmt_heap_gb(max_gb))
    } else {
        format!("{} to {} GB", fmt_heap_gb(min_gb), fmt_heap_gb(max_gb))
    }
}

fn fmt_heap_gb(gb: f32) -> String {
    if (gb.fract()).abs() < f32::EPSILON {
        format!("{}", gb as i32)
    } else {
        format!("{gb:.1}")
    }
}

fn performance_mode_label(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::Managed => "Managed",
        PerformanceMode::Vanilla => "Vanilla",
        PerformanceMode::Custom => "Custom",
    }
}

fn performance_mode_token(mode: PerformanceMode) -> &'static str {
    match mode {
        PerformanceMode::Managed => "managed",
        PerformanceMode::Vanilla => "vanilla",
        PerformanceMode::Custom => "custom",
    }
}

fn disabled_health_response(
    mode: PerformanceMode,
    display: PerformanceInstanceDisplay,
) -> PerformanceHealthResponse {
    PerformanceHealthResponse {
        active: false,
        health: BundleHealth::Disabled,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: Vec::new(),
        guardian_facts: Vec::new(),
        proof: performance_health_proof(
            None,
            BundleHealth::Disabled,
            "",
            "",
            0,
            0,
            RollbackState::NotApplicable,
        ),
        view_model: application::performance_plan_summary_view_model(
            mode,
            None,
            BundleHealth::Disabled,
            None,
            0,
            &[],
        ),
        display,
    }
}

fn invalid_health_response(
    warning: impl Into<String>,
    guardian_facts: Vec<GuardianFact>,
    display: PerformanceInstanceDisplay,
) -> PerformanceHealthResponse {
    let warning = warning.into();
    let warnings = vec![warning];
    PerformanceHealthResponse {
        active: true,
        health: BundleHealth::Invalid,
        composition_id: String::new(),
        tier: String::new(),
        installed_count: 0,
        managed_artifacts: Vec::new(),
        warnings: warnings.clone(),
        guardian_facts,
        proof: performance_health_proof(
            None,
            BundleHealth::Invalid,
            "",
            "",
            0,
            1,
            RollbackState::Unavailable,
        ),
        view_model: application::performance_plan_summary_view_model(
            PerformanceMode::Managed,
            None,
            BundleHealth::Invalid,
            None,
            0,
            &warnings,
        ),
        display,
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
        Some(_) => Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "invalid performance action" })),
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

fn resolve_instance_version_target(
    state: &AppState,
    instance: &croopor_config::Instance,
    game_version_override: Option<&str>,
    loader_override: Option<&str>,
) -> Result<(String, String), (StatusCode, Json<serde_json::Value>)> {
    let explicit_game_version = optional_value(game_version_override);
    let explicit_loader = optional_value(loader_override);
    if let Some(game_version) = explicit_game_version.clone()
        && let Some(loader) = explicit_loader.clone()
    {
        return Ok((game_version, loader));
    }

    let library_dir = state.library_dir().ok_or_else(|| {
        (
            StatusCode::PRECONDITION_FAILED,
            Json(serde_json::json!({ "error": "Croopor library is not configured" })),
        )
    })?;
    let versions = scan_versions(&std::path::PathBuf::from(library_dir)).map_err(|_| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(serde_json::json!({
                "error": "Could not scan installed versions. Check the library folder and try again."
            })),
        )
    })?;
    let version = versions
        .iter()
        .find(|version| version.id == instance.version_id)
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": "instance version metadata is unavailable; install the version before resolving performance files"
                })),
            )
        })?;

    let game_version = explicit_game_version.unwrap_or_else(|| {
        let parent = version.inherits_from.trim();
        if parent.is_empty() {
            version.id.clone()
        } else {
            parent.to_string()
        }
    });
    let loader = explicit_loader.unwrap_or_else(|| {
        version
            .loader
            .as_ref()
            .map(|loader| loader.component_id.short_key().to_string())
            .unwrap_or_else(|| "vanilla".to_string())
    });

    Ok((game_version, loader))
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
                Json(serde_json::json!({ "error": "invalid performance mode" })),
            )
        });
    }
    Ok(parse_mode(&state.config().current().performance_mode).unwrap_or(PerformanceMode::Managed))
}

fn resolve_instance_mode(
    state: &AppState,
    instance: &croopor_config::Instance,
    raw: Option<&str>,
) -> Result<PerformanceMode, (StatusCode, Json<serde_json::Value>)> {
    if let Some(raw) = raw.filter(|value| !value.trim().is_empty()) {
        return parse_mode(raw).ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "invalid performance mode" })),
            )
        });
    }
    if let Some(mode) = parse_mode(&instance.performance_mode) {
        return Ok(mode);
    }
    resolve_config_mode(state, None)
}

fn installed_mod_evidence(
    mods_dir: &std::path::Path,
    state: Option<&croopor_performance::CompositionState>,
) -> Vec<String> {
    let mut evidence = std::collections::BTreeSet::new();
    if let Some(state) = state {
        for installed in &state.installed_mods {
            add_mod_evidence(&mut evidence, &installed.project_id);
        }
    }
    for value in installed_mod_file_evidence(mods_dir) {
        evidence.insert(value);
    }
    evidence.into_iter().collect()
}

fn installed_mod_evidence_from_mods_dir(mods_dir: &std::path::Path) -> Vec<String> {
    let state = load_state(mods_dir).ok().flatten();
    installed_mod_evidence(mods_dir, state.as_ref())
}

fn installed_mod_file_evidence(mods_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(mods_dir) else {
        return Vec::new();
    };
    let mut evidence = std::collections::BTreeSet::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if !path
            .extension()
            .and_then(|value| value.to_str())
            .is_some_and(|value| value.eq_ignore_ascii_case("jar"))
        {
            continue;
        }
        if let Some(stem) = path.file_stem().and_then(|value| value.to_str()) {
            add_mod_evidence(&mut evidence, stem);
        }
    }
    evidence.into_iter().collect()
}

fn add_mod_evidence(evidence: &mut std::collections::BTreeSet<String>, raw: &str) {
    let normalized = raw.trim().to_lowercase();
    if normalized.is_empty() {
        return;
    }
    evidence.insert(normalized.clone());

    let mut prefix = String::new();
    for token in normalized
        .split(|value: char| !value.is_ascii_alphanumeric())
        .filter(|value| !value.is_empty())
    {
        if is_versionish_mod_filename_token(token) {
            break;
        }
        if prefix.is_empty() {
            prefix.push_str(token);
        } else {
            prefix.push('-');
            prefix.push_str(token);
        }
        evidence.insert(prefix.clone());
    }
}

fn is_versionish_mod_filename_token(token: &str) -> bool {
    token.strip_prefix("mc").is_some_and(|suffix| {
        suffix
            .as_bytes()
            .first()
            .is_some_and(|value| value.is_ascii_digit())
    }) || token.strip_prefix('v').is_some_and(|suffix| {
        suffix
            .as_bytes()
            .first()
            .is_some_and(|value| value.is_ascii_digit())
    }) || token
        .as_bytes()
        .first()
        .is_some_and(|value| value.is_ascii_digit())
}

fn response_warnings(plan: &CompositionPlan, health_warnings: Vec<String>) -> Vec<String> {
    let mut warnings = plan.warnings.clone();
    warnings.extend(health_warnings);
    warnings
}

fn internal_error(_error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": PERFORMANCE_DATA_INTERNAL_ERROR })),
    )
}

fn internal_install_error(_error: impl std::fmt::Display) -> (StatusCode, Json<serde_json::Value>) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": PERFORMANCE_INSTALL_INTERNAL_ERROR })),
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
        InstallError::State(StateError::InvalidIntegrity { .. }) => (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({
                "error": "invalid performance artifact integrity metadata"
            })),
        ),
        error => internal_install_error(error),
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
    use croopor_config::{AppConfig, AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::modrinth::ModrinthError;
    use croopor_performance::{CompositionState, InstalledMod, PerformanceManager};
    use ed25519_dalek::{Signer, SigningKey};
    use std::{
        fs,
        path::{Path as FsPath, PathBuf},
        sync::Arc,
        time::Duration,
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tower::ServiceExt;

    #[tokio::test]
    async fn status_reports_bundled_rules_without_remote_refresh() {
        let fixture = TestFixture::new("status");

        let Json(response) = handle_status(State(fixture.state.clone()))
            .await
            .expect("status should serialize");
        let status = &response.status;

        assert_eq!(status.rule_source, croopor_performance::RuleSource::BuiltIn);
        assert_eq!(
            status.rule_channel,
            croopor_performance::RuleChannel::Bundled
        );
        assert!(status.rules_cache.recorded);
        assert_eq!(
            status.rules_cache.state,
            croopor_performance::RulesCacheState::Recorded
        );
        assert!(status.rules_cache.updated_at.is_some());
        assert!(status.rules_cache.loaded_at.is_some());
        assert!(status.rules_cache.warning.is_none());
        assert_eq!(status.schema_version, 1);
        assert!(!status.generated_at.is_empty());
        assert!(status.composition_count > 0);
        assert!(!status.remote_refresh);
        assert_eq!(status.last_refresh_at, None);
        assert!(response.guardian_facts.is_empty());
        assert_eq!(
            status.validation,
            croopor_performance::RulesValidation::Valid
        );
        assert_eq!(
            status.health_states,
            vec![
                BundleHealth::Healthy,
                BundleHealth::Degraded,
                BundleHealth::Fallback,
                BundleHealth::Disabled,
                BundleHealth::Invalid,
            ]
        );
        assert_eq!(
            status.ownership_classes,
            vec![
                croopor_performance::OwnershipClass::CompositionManaged,
                croopor_performance::OwnershipClass::UserManaged,
            ]
        );
        assert!(status.warnings.is_empty());
    }

    #[tokio::test]
    async fn status_exposes_repeated_performance_failure_memory_fact() {
        let fixture = TestFixture::new("status-repeated-performance-memory");
        seed_repeated_performance_memory(&fixture.state, "family-f-fabric-core", 3);

        let Json(response) = handle_status(State(fixture.state.clone()))
            .await
            .expect("status should serialize");

        assert_eq!(
            response.status.rule_source,
            croopor_performance::RuleSource::BuiltIn
        );
        let fact = response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_repeated_failure_memory")
            .expect("repeated failure memory fact");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Performance);
        assert_eq!(
            fact.ownership,
            crate::state::contracts::OwnershipClass::CompositionManaged
        );
        assert_eq!(
            fact.target.as_ref().map(|target| target.id.as_str()),
            Some("family-f-fabric-core")
        );
        assert!(
            fact.fields
                .iter()
                .any(|field| field.key == "occurrence_count" && field.value == "3")
        );
    }

    #[tokio::test]
    async fn status_reports_invalid_remote_rules_with_guardian_fact_and_safe_copy() {
        let root = test_root("status-invalid-remote-rules");
        let paths = test_paths(&root);
        let mut remote = croopor_performance::builtin_manifest().expect("builtin manifest");
        remote.schema_version = 99;
        let signed = signed_rules_response(&remote);
        let cache_path = croopor_performance::rules_cache_path(&paths.config_dir);
        fs::create_dir_all(cache_path.parent().expect("cache parent")).expect("create cache dir");
        fs::write(
            &cache_path,
            serde_json::to_vec(&croopor_performance::RulesCacheSnapshot {
                rule_source: croopor_performance::RuleSource::Remote,
                rule_channel: croopor_performance::RuleChannel::Remote,
                schema_version: remote.schema_version,
                generated_at: remote.generated_at.clone(),
                validation: croopor_performance::RulesValidation::Valid,
                updated_at: "2026-06-15T12:00:00Z".to_string(),
                loaded_at: "2026-06-15T12:00:00Z".to_string(),
                manifest: Some(remote),
                signature: Some(croopor_performance::RulesSignatureMetadata {
                    signature: signed.signature,
                    key_id: Some("test-key".to_string()),
                }),
            })
            .expect("serialize invalid remote cache"),
        )
        .expect("write invalid remote cache");
        let remote_url =
            "https://rules.example.test/private-feed/performance.json?api_token=secret-token";
        let state = build_test_state(&root, Some(remote_url.to_string()), Some(signed.public_key));

        let response = router()
            .with_state(state)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/performance/status")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let body = String::from_utf8(body.to_vec()).expect("utf8 body");
        let value: serde_json::Value = serde_json::from_str(&body).expect("status json");

        assert_eq!(value["rule_source"], "built_in");
        assert_eq!(value["rule_channel"], "bundled");
        assert_eq!(value["rules_cache"]["state"], "invalid");
        assert!(
            value["warnings"]
                .as_array()
                .expect("warnings")
                .iter()
                .any(|warning| warning
                    .as_str()
                    .is_some_and(|warning| warning.contains("Remote rules cache was invalid")))
        );
        let fact = value["guardian_facts"]
            .as_array()
            .expect("guardian facts")
            .iter()
            .find(|fact| fact["id"] == "performance_rules_invalid")
            .expect("invalid rules fact");
        assert_eq!(fact["domain"], "Performance");
        assert_eq!(fact["severity"], "Degraded");
        assert_eq!(fact["confidence"], "High");
        assert_eq!(fact["ownership"], "ExternalProviderDerived");
        assert_omits_raw_fragments(
            &body,
            &[
                remote_url,
                "private-feed",
                "api_token",
                "secret-token",
                &cache_path.display().to_string(),
            ],
        );

        let _ = fs::remove_dir_all(root);
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
        let journal = fixture
            .state
            .journals()
            .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
            .expect("refresh journal");
        assert_eq!(
            journal.status,
            crate::state::contracts::OperationStatus::Failed
        );
        assert_eq!(
            journal.failure_point.as_deref(),
            Some("refresh_remote_rules")
        );
        assert_eq!(
            journal.outcome,
            Some(crate::state::contracts::OperationOutcome::Failed)
        );
        assert!(journal.targets.iter().any(|target| {
            target.id == "performance_rules_remote_source"
                && target.ownership
                    == crate::state::contracts::OwnershipClass::ExternalProviderDerived
        }));
        assert!(journal.targets.iter().any(|target| {
            target.id == "performance_rules_cache"
                && target.ownership == crate::state::contracts::OwnershipClass::LauncherManaged
        }));
    }

    #[tokio::test]
    async fn rules_refresh_route_accepts_configured_remote_manifest() {
        let mut manifest = croopor_performance::builtin_manifest().expect("builtin manifest");
        manifest.generated_at = "2026-05-30T13:00:00Z".to_string();
        let signed = signed_rules_response(&manifest);
        let remote_url = spawn_rules_server(
            serde_json::to_vec(&manifest).expect("serialize remote manifest"),
            Some(signed.signature),
        )
        .await;
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            "rules-refresh-configured",
            Some(remote_url),
            Some(signed.public_key),
        );

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
        let status: croopor_performance::PerformanceRulesStatus =
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
        let journal = fixture
            .state
            .journals()
            .latest_for_command(crate::state::contracts::CommandKind::RefreshPerformanceRules)
            .expect("refresh journal");
        assert_eq!(
            journal.status,
            crate::state::contracts::OperationStatus::Succeeded
        );
        assert_eq!(journal.failure_point, None);
        assert_eq!(
            journal.outcome,
            Some(crate::state::contracts::OperationOutcome::Succeeded)
        );
        assert_eq!(journal.planned_steps.len(), 1);
        assert_eq!(journal.completed_steps.len(), 1);
        assert!(journal.targets.iter().any(|target| {
            target.id == "performance_rules_remote_source"
                && target.ownership
                    == crate::state::contracts::OwnershipClass::ExternalProviderDerived
        }));
        assert!(journal.targets.iter().any(|target| {
            target.id == "performance_rules_cache"
                && target.ownership == crate::state::contracts::OwnershipClass::LauncherManaged
        }));
        assert_eq!(
            journal.completed_steps[0]
                .changed_target
                .as_ref()
                .map(|target| target.ownership),
            Some(crate::state::contracts::OwnershipClass::LauncherManaged)
        );
    }

    #[tokio::test]
    async fn rules_refresh_route_rejects_missing_signature_and_keeps_builtin_rules() {
        let mut manifest = croopor_performance::builtin_manifest().expect("builtin manifest");
        manifest.generated_at = "2026-05-30T13:30:00Z".to_string();
        let signed = signed_rules_response(&manifest);
        let remote_url = spawn_rules_server(
            serde_json::to_vec(&manifest).expect("serialize remote manifest"),
            None,
        )
        .await;
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            "rules-refresh-missing-signature",
            Some(remote_url),
            Some(signed.public_key),
        );

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
        let status: croopor_performance::PerformanceRulesStatus =
            serde_json::from_slice(&body).expect("rules status json");
        assert_eq!(status.rule_source, croopor_performance::RuleSource::BuiltIn);
        assert!(status.remote_refresh);
        assert!(
            status
                .warnings
                .iter()
                .any(|warning| warning.contains("signature header is missing"))
        );
    }

    #[test]
    fn bounded_performance_data_error_omits_raw_internal_details() {
        let raw_parser = serde_json::from_str::<serde_json::Value>("{not json")
            .expect_err("invalid json")
            .to_string();
        let raw_error = format!(
            "failed to read /home/zero/.config/croopor/performance.json and C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\performance.json: {raw_parser}: Permission denied (os error 13)"
        );

        let error = internal_error(&raw_error);
        let body = json_error_message(&error);

        assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(body, PERFORMANCE_DATA_INTERNAL_ERROR);
        assert_omits_raw_fragments(
            &body,
            &[
                "/home/zero/.config/croopor/performance.json",
                "C:\\Users\\Zero\\AppData\\Roaming\\Croopor\\performance.json",
                raw_parser.as_str(),
                "Permission denied",
                "os error 13",
            ],
        );
    }

    #[test]
    fn bounded_install_errors_omit_raw_provider_artifact_and_os_details() {
        let cases = [
            performance_install_error(InstallError::Modrinth(ModrinthError::Http {
                status: 500,
                body: "provider failure from https://api.modrinth.com/v2/project/sodium/version"
                    .to_string(),
            })),
            performance_install_error(InstallError::ManagedArtifactTargetExists(
                "sodium-fabric-mc1.20.4-0.5.8.jar".to_string(),
            )),
            performance_install_error(InstallError::Io(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "Permission denied (os error 13)",
            ))),
        ];

        for error in cases {
            let body = json_error_message(&error);

            assert_eq!(error.0, StatusCode::INTERNAL_SERVER_ERROR);
            assert_eq!(body, PERFORMANCE_INSTALL_INTERNAL_ERROR);
            assert_omits_raw_fragments(
                &body,
                &[
                    "https://api.modrinth.com",
                    "modrinth",
                    "sodium-fabric-mc1.20.4-0.5.8.jar",
                    "Permission denied",
                    "os error 13",
                ],
            );
        }
    }

    #[test]
    fn health_parse_warning_omits_raw_parser_text() {
        let raw_parser = serde_json::from_str::<serde_json::Value>("{not json")
            .expect_err("invalid json")
            .to_string();
        let response = invalid_health_response(
            PERFORMANCE_STATE_PARSE_WARNING,
            Vec::new(),
            test_performance_display(),
        );
        let warnings = response.warnings.join("\n");

        assert_eq!(warnings, PERFORMANCE_STATE_PARSE_WARNING);
        assert!(!warnings.contains(&raw_parser));
        assert!(response.guardian_facts.is_empty());
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
                instance_id: None,
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
                mode: Some(r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string()),
                instance_id: None,
            }),
        )
        .await
        .expect_err("invalid mode should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        let body = serde_json::to_string(&error.1.0).expect("error json");
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid performance mode" })
        );
        assert_omits_raw_fragments(
            &body,
            &[
                "C:\\Users\\Alice",
                ".minecraft",
                "--accessToken",
                "raw-secret",
            ],
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
                instance_id: None,
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
    async fn plan_effective_contract_covers_managed_vanilla_and_custom_modes() {
        let fixture = TestFixture::new("plan-effective-contract-modes");

        for (raw_mode, expected_active) in
            [("managed", true), ("vanilla", false), ("custom", false)]
        {
            let Json(response) = handle_plan(
                State(fixture.state.clone()),
                Query(PlanQuery {
                    game_version: Some("1.20.4".to_string()),
                    loader: Some("fabric".to_string()),
                    mode: Some(raw_mode.to_string()),
                    instance_id: None,
                }),
            )
            .await
            .expect("effective plan should serialize");

            assert_eq!(response.active, expected_active);
            assert_eq!(response.effective.active, expected_active);
            assert_eq!(response.effective.selected_mode, response.plan.mode);
            assert_eq!(response.effective.version_family, response.plan.family);
            assert_eq!(response.effective.loader, response.plan.loader);
            assert!(!response.effective.explanation.summary.trim().is_empty());
            assert!(
                response.effective.explanation.summary.len() <= 180,
                "{}",
                response.effective.explanation.summary
            );
        }
    }

    #[tokio::test]
    async fn plan_route_preserves_flat_legacy_fields_and_adds_effective_payload() {
        let fixture = TestFixture::new("plan-effective-route-shape");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/performance/plan?game_version=1.20.4&loader=fabric&mode=managed")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read body");
        let value: serde_json::Value = serde_json::from_slice(&body).expect("plan json");

        assert_eq!(value["active"], true);
        assert_eq!(value["mode"], "managed");
        assert!(value.get("composition_id").is_some());
        assert!(value["guardian_facts"].is_array());
        assert_eq!(value["effective"]["active"], true);
        assert_eq!(value["effective"]["selected_mode"], "managed");
        assert_eq!(value["effective"]["version_family"], value["family"]);
        assert_eq!(value["effective"]["loader"], value["loader"]);
        assert!(value["effective"]["composition"]["tier"].is_string());
        assert!(value["effective"]["managed_artifacts"].is_array());
        assert!(
            value["effective"]["explanation"]["summary"]
                .as_str()
                .is_some_and(|summary| !summary.trim().is_empty())
        );
    }

    #[tokio::test]
    async fn plan_effective_contract_preserves_hyphenated_family_d_composition_id() {
        let fixture = TestFixture::new("plan-effective-family-d-identity");

        let Json(response) = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.15.2".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some("managed".to_string()),
                instance_id: None,
            }),
        )
        .await
        .expect("family d plan should serialize");

        assert_eq!(response.plan.composition_id, "family-d-vanilla-enhanced");
        assert!(response.effective.composition.selected);
        assert_eq!(
            response.effective.composition.id.as_deref(),
            Some(response.plan.composition_id.as_str())
        );
    }

    #[tokio::test]
    async fn plan_missing_instance_returns_json_error() {
        let fixture = TestFixture::new("plan-missing-instance");

        let error = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.20.4".to_string()),
                loader: Some("fabric".to_string()),
                mode: None,
                instance_id: Some("missing".to_string()),
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
    async fn plan_without_instance_id_stays_request_only() {
        let manifest = nvidium_always_manifest("2026-05-30T14:10:00Z");
        let signed = signed_rules_response(&manifest);
        let remote_url = spawn_rules_server(
            serde_json::to_vec(&manifest).expect("serialize remote manifest"),
            Some(signed.signature),
        )
        .await;
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            "plan-request-only-iris",
            Some(remote_url),
            Some(signed.public_key),
        );
        let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
            .await
            .expect("remote manifest should refresh");
        assert_eq!(
            status.status.rule_source,
            croopor_performance::RuleSource::Remote
        );
        assert!(status.guardian_facts.is_empty());
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

        let Json(response) = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.20.4".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some("managed".to_string()),
                instance_id: None,
            }),
        )
        .await
        .expect("request-only plan should serialize");

        assert!(
            response
                .plan
                .mods
                .iter()
                .any(|managed_mod| managed_mod.slug == "nvidium")
        );
        assert!(response.plan.warnings.is_empty());
    }

    #[tokio::test]
    async fn plan_with_instance_id_uses_user_installed_iris_file_for_nvidium_exclusion() {
        let manifest = nvidium_always_manifest("2026-05-30T14:20:00Z");
        let signed = signed_rules_response(&manifest);
        let remote_url = spawn_rules_server(
            serde_json::to_vec(&manifest).expect("serialize remote manifest"),
            Some(signed.signature),
        )
        .await;
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            "plan-iris-nvidium-exclusion",
            Some(remote_url),
            Some(signed.public_key),
        );
        let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
            .await
            .expect("remote manifest should refresh");
        assert_eq!(
            status.status.rule_source,
            croopor_performance::RuleSource::Remote
        );
        assert!(status.guardian_facts.is_empty());
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

        let Json(response) = handle_plan(
            State(fixture.state.clone()),
            Query(PlanQuery {
                game_version: Some("1.20.4".to_string()),
                loader: Some("fabric".to_string()),
                mode: Some("managed".to_string()),
                instance_id: Some(instance_id),
            }),
        )
        .await
        .expect("instance-scoped plan should serialize");

        assert!(
            response
                .plan
                .mods
                .iter()
                .all(|managed_mod| managed_mod.slug != "nvidium")
        );
        assert!(
            response.plan.warnings.iter().any(|warning| {
                warning == "nvidium skipped: incompatible with managed mod iris"
            })
        );
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
        fixture.write_fabric_version("1.20.4-fabric", "1.20.4");
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
                    source: test_modrinth_source(),
                    integrity: croopor_performance::ManagedArtifactIntegrity {
                        sha512: valid_sha512(),
                        sha512_verified: true,
                    },
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
                source_provider: croopor_performance::ManagedArtifactProvider::Modrinth,
                sha512_present: true,
                sha512_verified: true,
            }]
        );
        let value = serde_json::to_value(&response).expect("serialize response");
        assert!(value.get("managed_artifacts").is_some());
        assert!(value.to_string().contains("managed.jar"));
        assert!(!value.to_string().contains(&mods_dir.display().to_string()));
        assert!(!value.to_string().contains(&valid_sha512()));
        assert_eq!(response.proof.health, bundle_health_token(response.health));
        assert_eq!(
            response.proof.target.ownership,
            crate::state::contracts::OwnershipClass::CompositionManaged
        );
        assert!(
            response
                .proof
                .fields
                .iter()
                .any(|field| { field.key == "managed_artifact_count" && field.value == "1" })
        );
        assert!(
            !serde_json::to_string(&response.proof)
                .expect("serialize proof")
                .contains("managed.jar")
        );
        assert!(!response.view_model.title.trim().is_empty());
        assert!(!response.view_model.detail.trim().is_empty());
        assert_eq!(response.view_model.managed_artifact_count, 1);
        assert_eq!(
            response.view_model.health.as_deref(),
            Some(bundle_health_token(response.health))
        );
        assert!(
            !serde_json::to_string(&response.view_model)
                .expect("serialize view model")
                .contains("managed.jar")
        );
        assert_eq!(response.display.memory.label, "0.5 to 4 GB");
        assert_eq!(response.display.runtime.label, "Java 21");
        assert!(response.display.runtime.detected);
        assert_eq!(response.display.mode.mode, "managed");
        assert_eq!(response.display.mode.source, "global");
    }

    #[tokio::test]
    async fn health_response_exposes_degraded_and_fallback_guardian_view_models_and_proofs() {
        let degraded = TestFixture::new("health-degraded-contract");
        let degraded_instance = degraded.add_instance("Managed", "1.20.4-fabric");
        degraded.write_fabric_version("1.20.4-fabric", "1.20.4");
        let degraded_mods_dir = degraded
            .state
            .instances()
            .game_dir(&degraded_instance)
            .join("mods");
        fs::create_dir_all(&degraded_mods_dir).expect("create degraded mods dir");
        fs::write(degraded_mods_dir.join("managed.jar"), b"managed")
            .expect("write degraded managed file");
        let mut degraded_state = test_composition_state(
            "family-f-fabric-core",
            vec![InstalledMod {
                project_id: "sodium".to_string(),
                version_id: "version-a".to_string(),
                filename: "managed.jar".to_string(),
                ownership_class: croopor_performance::OwnershipClass::CompositionManaged,
                source: test_modrinth_source(),
                integrity: croopor_performance::ManagedArtifactIntegrity {
                    sha512: valid_sha512(),
                    sha512_verified: true,
                },
            }],
        );
        degraded_state.failure_count = 1;
        degraded_state.last_failure = "managed artifact install failed".to_string();
        croopor_performance::state::save_state(&degraded_mods_dir, &degraded_state)
            .expect("save degraded state");
        croopor_performance::state::save_rollback_snapshot(&degraded_mods_dir, &degraded_state)
            .expect("save degraded rollback snapshot");

        let Json(degraded_response) = handle_health(
            State(degraded.state.clone()),
            Query(HealthQuery {
                instance_id: Some(degraded_instance),
            }),
        )
        .await
        .expect("degraded health should serialize");

        assert_eq!(degraded_response.health, BundleHealth::Degraded);
        assert_eq!(degraded_response.proof.health, "degraded");
        assert_eq!(degraded_response.proof.rollback, RollbackState::Available);
        assert_eq!(
            degraded_response.view_model.state_id,
            "performance_summary_degraded"
        );
        assert_eq!(
            degraded_response.view_model.health.as_deref(),
            Some("degraded")
        );
        let degraded_fact = degraded_response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_health_degraded")
            .expect("degraded Guardian fact");
        assert_eq!(
            degraded_fact.ownership,
            crate::state::contracts::OwnershipClass::CompositionManaged
        );
        assert_eq!(
            degraded_fact.severity,
            Some(crate::guardian::GuardianSeverity::Degraded)
        );

        let fallback = TestFixture::new("health-fallback-contract");
        let fallback_instance = fallback.add_instance("Managed", "1.20.4-fabric");
        fallback.write_fabric_version("1.20.4-fabric", "1.20.4");
        let fallback_mods_dir = fallback
            .state
            .instances()
            .game_dir(&fallback_instance)
            .join("mods");
        fs::create_dir_all(&fallback_mods_dir).expect("create fallback mods dir");
        let fallback_state = CompositionState {
            composition_id: "family-f-vanilla-enhanced".to_string(),
            tier: CompositionTier::VanillaEnhanced,
            installed_mods: Vec::new(),
            installed_at: "2026-05-30T00:00:00Z".to_string(),
            failure_count: 0,
            last_failure: String::new(),
        };
        croopor_performance::state::save_state(&fallback_mods_dir, &fallback_state)
            .expect("save fallback state");
        croopor_performance::state::save_rollback_snapshot(&fallback_mods_dir, &fallback_state)
            .expect("save fallback rollback snapshot");

        let Json(fallback_response) = handle_health(
            State(fallback.state.clone()),
            Query(HealthQuery {
                instance_id: Some(fallback_instance),
            }),
        )
        .await
        .expect("fallback health should serialize");

        assert_eq!(fallback_response.health, BundleHealth::Fallback);
        assert_eq!(fallback_response.proof.health, "fallback");
        assert_eq!(fallback_response.proof.rollback, RollbackState::Available);
        assert_eq!(
            fallback_response.view_model.state_id,
            "performance_summary_fallback"
        );
        assert_eq!(
            fallback_response.view_model.health.as_deref(),
            Some("fallback")
        );
        let fallback_fact = fallback_response
            .guardian_facts
            .iter()
            .find(|fact| fact.id.as_str() == "performance_health_fallback")
            .expect("fallback Guardian fact");
        assert_eq!(
            fallback_fact.ownership,
            crate::state::contracts::OwnershipClass::CompositionManaged
        );
        assert_eq!(
            fallback_fact.severity,
            Some(crate::guardian::GuardianSeverity::Warning)
        );
    }

    #[tokio::test]
    async fn health_plan_uses_user_installed_iris_file_for_nvidium_exclusion() {
        let mut manifest = croopor_performance::builtin_manifest().expect("builtin manifest");
        manifest.generated_at = "2026-05-30T14:00:00Z".to_string();
        for composition in &mut manifest.compositions {
            for managed_mod in &mut composition.mods {
                if managed_mod.slug == "nvidium" {
                    managed_mod.condition = croopor_performance::types::ModCondition::Always;
                    managed_mod.hardware_req = None;
                }
            }
        }
        let signed = signed_rules_response(&manifest);
        let remote_url = spawn_rules_server(
            serde_json::to_vec(&manifest).expect("serialize remote manifest"),
            Some(signed.signature),
        )
        .await;
        let fixture = TestFixture::new_with_remote_url_and_public_key(
            "health-iris-nvidium-exclusion",
            Some(remote_url),
            Some(signed.public_key),
        );
        let Json(status) = handle_rules_refresh(State(fixture.state.clone()))
            .await
            .expect("remote manifest should refresh");
        assert_eq!(
            status.status.rule_source,
            croopor_performance::RuleSource::Remote
        );
        assert!(status.guardian_facts.is_empty());
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        fixture.write_fabric_version("1.20.4-fabric", "1.20.4");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");

        let Json(response) = handle_health(
            State(fixture.state.clone()),
            Query(HealthQuery {
                instance_id: Some(instance_id),
            }),
        )
        .await
        .expect("managed health should serialize");

        assert!(response.active);
        assert!(
            response.warnings.iter().any(|warning| {
                warning == "nvidium skipped: incompatible with managed mod iris"
            })
        );
    }

    #[test]
    fn installed_mod_evidence_preserves_state_ids_and_jar_name_tokens() {
        let mods_dir = test_root("installed-mod-evidence");
        fs::write(mods_dir.join("iris-mc1.20.1-1.7.0.jar"), b"iris").expect("write iris jar");
        fs::write(mods_dir.join("notes.txt"), b"not a jar").expect("write text file");
        let state =
            test_composition_state("core", vec![test_installed_mod("sodium", "sodium.jar")]);

        let evidence = installed_mod_evidence(&mods_dir, Some(&state));

        assert!(evidence.contains(&"sodium".to_string()));
        assert!(evidence.contains(&"iris".to_string()));
        assert!(evidence.contains(&"iris-mc1.20.1-1.7.0".to_string()));
        assert!(!evidence.contains(&"notes".to_string()));

        let _ = fs::remove_dir_all(&mods_dir);
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
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
        assert_eq!(response.guardian_facts.len(), 1);
        let fact = &response.guardian_facts[0];
        assert_eq!(fact.id.as_str(), "performance_user_owned_conflict");
        assert_eq!(fact.domain, crate::guardian::GuardianDomain::Performance);
        assert_eq!(
            fact.severity,
            Some(crate::guardian::GuardianSeverity::Blocking)
        );
        assert_eq!(
            fact.confidence,
            Some(crate::guardian::GuardianConfidence::Confirmed)
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
    async fn install_invalid_action_returns_redacted_json_error() {
        let fixture = TestFixture::new("install-invalid-action");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: None,
                action: Some("/Users/alice/.minecraft --accessToken raw-secret".to_string()),
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("invalid action should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        let body = serde_json::to_string(&error.1.0).expect("error json");
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid performance action" })
        );
        assert_omits_raw_fragments(
            &body,
            &["/Users/alice", ".minecraft", "--accessToken", "raw-secret"],
        );
    }

    #[tokio::test]
    async fn install_invalid_mode_returns_redacted_json_error() {
        let fixture = TestFixture::new("install-invalid-mode");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let error = handle_install(
            State(fixture.state.clone()),
            Json(InstallRequest {
                instance_id: Some(instance_id),
                game_version: None,
                loader: None,
                mode: Some(r"C:\Users\Alice\.minecraft --accessToken raw-secret".to_string()),
                action: None,
                rollback_id: None,
                queued: None,
            }),
        )
        .await
        .expect_err("invalid mode should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        let body = serde_json::to_string(&error.1.0).expect("error json");
        assert_eq!(
            error.1.0,
            serde_json::json!({ "error": "invalid performance mode" })
        );
        assert_omits_raw_fragments(
            &body,
            &[
                "C:\\Users\\Alice",
                ".minecraft",
                "--accessToken",
                "raw-secret",
            ],
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
                    source: test_modrinth_source(),
                    integrity: croopor_performance::ManagedArtifactIntegrity {
                        sha512: String::new(),
                        sha512_verified: false,
                    },
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
        let journal = fixture
            .state
            .journals()
            .latest_for_command(crate::state::contracts::CommandKind::ApplyPerformancePlan)
            .expect("remove journal");
        assert_eq!(
            journal.status,
            crate::state::contracts::OperationStatus::Succeeded
        );
        assert_eq!(
            journal.rollback,
            crate::state::contracts::RollbackState::Available
        );
        assert!(journal.targets.iter().any(|target| {
            target.id == "core"
                && target.ownership == crate::state::contracts::OwnershipClass::CompositionManaged
        }));
        assert_eq!(journal.completed_steps.len(), 1);
        assert_eq!(
            journal.completed_steps[0].step_id,
            "remove_performance_plan"
        );
        assert_eq!(
            journal.completed_steps[0]
                .changed_target
                .as_ref()
                .map(|target| (target.id.as_str(), target.ownership)),
            Some((
                "core",
                crate::state::contracts::OwnershipClass::CompositionManaged
            ))
        );
        assert!(
            journal.completed_steps[0]
                .generated_facts
                .contains(&"performance_rollback_evidence".to_string())
        );
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
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "", "sha512_verified": false }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
    async fn install_remove_rejects_invalid_integrity_without_deleting_files() {
        let fixture = TestFixture::new("install-invalid-integrity-remove");
        let instance_id = fixture.add_instance("Custom", "1.20.4-fabric");
        let mods_dir = fixture
            .state
            .instances()
            .game_dir(&instance_id)
            .join("mods");
        fs::create_dir_all(&mods_dir).expect("create mods dir");
        fs::write(mods_dir.join("managed.jar"), b"managed").expect("write managed file");
        fs::write(
            mods_dir.join(".croopor-lock.json"),
            serde_json::to_vec(&serde_json::json!({
                "composition_id": "core",
                "tier": "core",
                "installed_mods": [{
                    "project_id": "sodium",
                    "version_id": "version",
                    "filename": "managed.jar",
                    "ownership_class": "composition_managed",
                    "source": { "provider": "modrinth" },
                    "integrity": { "sha512": "abc123", "sha512_verified": true }
                }],
                "installed_at": "2026-05-30T00:00:00Z",
                "failure_count": 0,
                "last_failure": ""
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
        .expect_err("invalid integrity should fail");

        assert_eq!(error.0, StatusCode::BAD_REQUEST);
        assert_eq!(
            error.1.0,
            serde_json::json!({
                "error": "invalid performance artifact integrity metadata"
            })
        );
        assert_eq!(
            fs::read(mods_dir.join("managed.jar")).expect("read managed"),
            b"managed"
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
                && snapshot["artifact_count"] == 1
                && snapshot["ownership_class"] == "composition_managed"
                && snapshot["rollback_available"] == true
                && snapshot["latest"] == false
        }));
        assert!(snapshots.iter().any(|snapshot| {
            snapshot["id"] == second.id
                && snapshot["composition_id"] == "core-b"
                && snapshot["artifact_count"] == 1
                && snapshot["ownership_class"] == "composition_managed"
                && snapshot["rollback_available"] == true
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
                source_provider: croopor_performance::ManagedArtifactProvider::Modrinth,
                sha512_present: false,
                sha512_verified: false,
            }]
        );
        assert_eq!(
            fs::read(mods_dir.join("managed-a.jar")).expect("read managed a"),
            b"managed-a"
        );
        assert!(!mods_dir.join("managed-b.jar").exists());
        let journal = fixture
            .state
            .journals()
            .latest_for_command(crate::state::contracts::CommandKind::ApplyPerformancePlan)
            .expect("rollback journal");
        assert_eq!(
            journal.status,
            crate::state::contracts::OperationStatus::Succeeded
        );
        assert_eq!(
            journal.rollback,
            crate::state::contracts::RollbackState::Available
        );
        assert_eq!(journal.completed_steps.len(), 1);
        assert_eq!(
            journal.completed_steps[0].rollback,
            crate::state::contracts::RollbackState::Applied
        );
        assert_eq!(
            journal.completed_steps[0]
                .changed_target
                .as_ref()
                .map(|target| (target.id.as_str(), target.ownership)),
            Some((
                "core-a",
                crate::state::contracts::OwnershipClass::CompositionManaged
            ))
        );
        assert!(
            journal.completed_steps[0]
                .generated_facts
                .contains(&"performance_rollback_evidence".to_string())
        );
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
            .start(
                instance_id.clone(),
                "remove".to_string(),
                test_operation_payload(),
            )
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
            .start(
                "instance-a".to_string(),
                "install".to_string(),
                test_operation_payload(),
            )
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
    async fn operation_status_routes_redact_payload_and_error_details() {
        let fixture = TestFixture::new("operation-status-redaction");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");
        let started = fixture
            .state
            .performance_operations()
            .start(
                instance_id.clone(),
                "install/provider_payload=secret-token".to_string(),
                PerformanceOperationPayload {
                    game_version: Some("/Users/alice/.minecraft/private-version".to_string()),
                    loader: Some("fabric".to_string()),
                    mode: Some("managed --accessToken secret-token".to_string()),
                    rollback_id: Some("rb-old\\secret".to_string()),
                },
            )
            .await
            .expect("operation starts");
        fixture
            .state
            .performance_operations()
            .record_failed(
                &started.id,
                "provider_payload={\"url\":\"https://cdn.example.test/private-provider/sodium-secret.jar?token=secret-token\"}; java_path=C:\\Users\\Alice\\Java\\bin\\java.exe; -Xmx8192M",
            )
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
        let body = String::from_utf8(body.to_vec()).expect("utf8 body");
        let value: serde_json::Value = serde_json::from_str(&body).expect("operation json");

        assert_eq!(value["state"], "failed");
        assert_eq!(value["action"], "unknown");
        assert_eq!(value["error"], "performance operation failed");
        assert_eq!(value["payload"]["game_version"], "redacted");
        assert_eq!(value["payload"]["loader"], "fabric");
        assert_eq!(value["payload"]["mode"], "redacted");
        assert_eq!(value["payload"]["rollback_id"], "redacted");
        assert_omits_raw_fragments(
            &body,
            &[
                "/Users/alice",
                "C:\\Users\\Alice",
                "provider_payload",
                "private-provider",
                "sodium-secret.jar",
                "secret-token",
                "-Xmx8192M",
            ],
        );

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/performance/instances/{instance_id}/operation"
                    ))
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("instance route response");

        assert_eq!(response.status(), StatusCode::OK);
        let body = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("read instance body");
        let body = String::from_utf8(body.to_vec()).expect("utf8 instance body");
        let value: serde_json::Value =
            serde_json::from_str(&body).expect("instance operation json");

        assert_eq!(value["operation"]["id"], started.id);
        assert_eq!(value["operation"]["action"], "unknown");
        assert_eq!(value["operation"]["error"], "performance operation failed");
        assert_eq!(value["operation"]["payload"]["game_version"], "redacted");
        assert_eq!(value["operation"]["payload"]["loader"], "fabric");
        assert_eq!(value["operation"]["payload"]["mode"], "redacted");
        assert_eq!(value["operation"]["payload"]["rollback_id"], "redacted");
        assert_omits_raw_fragments(
            &body,
            &[
                "/Users/alice",
                "C:\\Users\\Alice",
                "provider_payload",
                "private-provider",
                "sodium-secret.jar",
                "secret-token",
                "-Xmx8192M",
            ],
        );
    }

    #[tokio::test]
    async fn instance_operation_route_returns_null_when_none_exists() {
        let fixture = TestFixture::new("instance-operation-empty");
        let instance_id = fixture.add_instance("Managed", "1.20.4-fabric");

        let response = router()
            .with_state(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/performance/instances/{instance_id}/operation"
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
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("operation response json");
        assert_eq!(value, serde_json::json!({ "operation": null }));
    }

    #[tokio::test]
    async fn instance_operation_route_discovers_reloaded_pending_operation() {
        let root = test_root("instance-operation-reloaded");
        let state = build_test_state(&root, None, None);
        let instance_id = state
            .instances()
            .add(
                "Managed".to_string(),
                "1.20.4-fabric".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance")
            .id;
        let started = state
            .performance_operations()
            .start(
                instance_id.clone(),
                "remove".to_string(),
                test_operation_payload(),
            )
            .await
            .expect("persist pending operation");
        state
            .performance_operations()
            .record_progress(&started.id, "removing")
            .await;
        drop(state);

        let reloaded = build_test_state(&root, None, None);
        let response = router()
            .with_state(reloaded)
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/api/v1/performance/instances/{instance_id}/operation"
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
        let value: serde_json::Value =
            serde_json::from_slice(&body).expect("operation response json");
        assert_eq!(value["operation"]["id"], started.id);
        assert_eq!(value["operation"]["instance_id"], instance_id);
        assert_eq!(value["operation"]["state"], "removing");

        let _ = fs::remove_dir_all(&root);
    }

    #[tokio::test]
    async fn startup_resume_runs_persisted_pending_remove_operation() {
        let root = test_root("startup-resume-remove");
        let state = build_test_state(&root, None, None);
        let instance_id = state
            .instances()
            .add(
                "Managed".to_string(),
                "1.20.4-fabric".to_string(),
                String::new(),
                String::new(),
                None,
            )
            .expect("add instance")
            .id;
        let started = state
            .performance_operations()
            .start(
                instance_id.clone(),
                "remove".to_string(),
                test_operation_payload(),
            )
            .await
            .expect("persist pending operation");
        state
            .performance_operations()
            .record_progress(&started.id, "removing")
            .await;
        drop(state);

        let reloaded = build_test_state(&root, None, None);
        let loaded = reloaded
            .performance_operations()
            .get(&started.id)
            .await
            .expect("pending operation should reload");
        assert_eq!(loaded.state, "removing");

        let resumed = resume_pending_performance_operations(reloaded.clone()).await;
        assert_eq!(resumed, 1);
        let events = collect_install_events(&reloaded, &started.id).await;
        let phases = events
            .iter()
            .map(|event| event.phase.as_str())
            .collect::<Vec<_>>();
        assert_eq!(phases, vec!["queued", "planning", "removing", "complete"]);
        let completed = reloaded
            .performance_operations()
            .get(&started.id)
            .await
            .expect("completed operation status");
        assert_eq!(completed.instance_id, instance_id);
        assert_eq!(completed.state, "complete");
        assert_eq!(completed.error, None);

        let _ = fs::remove_dir_all(&root);
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

    fn json_error_message(error: &(StatusCode, Json<serde_json::Value>)) -> String {
        error
            .1
            .0
            .get("error")
            .and_then(|value| value.as_str())
            .expect("json error message")
            .to_string()
    }

    fn assert_omits_raw_fragments(body: &str, fragments: &[&str]) {
        for fragment in fragments {
            assert!(
                !body.contains(fragment),
                "bounded error body should not contain {fragment:?}: {body}"
            );
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
            Self::new_with_remote_url_and_public_key(name, remote_rules_url, None)
        }

        fn new_with_remote_url_and_public_key(
            name: &str,
            remote_rules_url: Option<String>,
            remote_rules_public_key: Option<String>,
        ) -> Self {
            let root = test_root(name);
            let state = build_test_state(&root, remote_rules_url, remote_rules_public_key);

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

        fn write_fabric_version(&self, version_id: &str, minecraft_version: &str) {
            let version_dir = self.root.join("library").join("versions").join(version_id);
            fs::create_dir_all(&version_dir).expect("create version dir");
            fs::write(
                version_dir.join(format!("{version_id}.json")),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "id": version_id,
                    "inheritsFrom": minecraft_version,
                    "type": "release",
                    "mainClass": "net.minecraft.client.main.Main",
                    "assetIndex": {},
                    "javaVersion": { "component": "java-runtime-delta", "majorVersion": 21 },
                    "libraries": []
                }))
                .expect("serialize version"),
            )
            .expect("write version json");
            fs::write(
                version_dir.join(".croopor-loader.json"),
                serde_json::to_vec_pretty(&serde_json::json!({
                    "schema_version": 1,
                    "component_id": "net.fabricmc.fabric-loader",
                    "component_name": "Fabric",
                    "build_id": format!("fabric:{minecraft_version}:0.16.10"),
                    "minecraft_version": minecraft_version,
                    "loader_version": "0.16.10",
                    "build_meta": {}
                }))
                .expect("serialize loader metadata"),
            )
            .expect("write loader metadata");
            fs::write(version_dir.join(format!("{version_id}.jar")), b"client jar")
                .expect("write version jar");
        }
    }

    async fn spawn_rules_server(body: Vec<u8>, signature: Option<String>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind rules server");
        let addr = listener.local_addr().expect("rules server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept rules request");
            let mut request = [0_u8; 1024];
            let _ = socket.read(&mut request).await;
            let signature_header = signature
                .as_ref()
                .map(|signature| {
                    format!(
                        "{}: {}\r\n{}: test-key\r\n",
                        croopor_performance::RULES_SIGNATURE_HEADER,
                        signature,
                        croopor_performance::RULES_KEY_ID_HEADER
                    )
                })
                .unwrap_or_default();
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n",
                signature_header,
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

    fn build_test_state(
        root: &FsPath,
        remote_rules_url: Option<String>,
        remote_rules_public_key: Option<String>,
    ) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        config
            .replace_in_memory(AppConfig {
                library_dir: paths.library_dir.to_string_lossy().to_string(),
                ..config.current()
            })
            .expect("configure library dir");
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::new_with_config_dir_remote_url_and_public_key(
                    &paths.config_dir,
                    remote_rules_url,
                    remote_rules_public_key,
                )
                .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
            frontend_dir: root.join("frontend"),
        })
    }

    fn seed_repeated_performance_memory(state: &AppState, composition_id: &str, count: u32) {
        let target = crate::state::contracts::TargetDescriptor::new(
            crate::state::contracts::StabilizationSystem::Performance,
            crate::state::contracts::TargetKind::PerformanceComposition,
            composition_id,
            crate::state::contracts::OwnershipClass::CompositionManaged,
        );
        let mut entry = crate::state::failure_memory::GuardianFailureMemoryEntry::observed(
            crate::guardian::DiagnosisId::new("performance_fallback_selected"),
            crate::guardian::GuardianDomain::Performance,
            target,
            crate::guardian::GuardianMode::Managed,
            Some("intent"),
            "2026-06-15T12:00:00Z",
        );
        entry.occurrence_count = count;
        state
            .failure_memory()
            .record(entry)
            .expect("record performance failure memory");
    }

    fn test_operation_payload() -> PerformanceOperationPayload {
        PerformanceOperationPayload {
            game_version: None,
            loader: None,
            mode: None,
            rollback_id: None,
        }
    }

    fn test_performance_display() -> PerformanceInstanceDisplay {
        PerformanceInstanceDisplay {
            memory: PerformanceMemoryDisplay {
                min_gb: 1.0,
                max_gb: 4.0,
                label: "1 to 4 GB".to_string(),
            },
            runtime: PerformanceRuntimeDisplay {
                detected: true,
                label: "Java 17".to_string(),
            },
            mode: PerformanceModeDisplay {
                mode: "managed".to_string(),
                label: "Managed".to_string(),
                source: "global".to_string(),
                source_label: "Global default".to_string(),
            },
        }
    }

    struct SignedRulesResponse {
        public_key: String,
        signature: String,
    }

    fn nvidium_always_manifest(generated_at: &str) -> croopor_performance::Manifest {
        let mut manifest = croopor_performance::builtin_manifest().expect("builtin manifest");
        manifest.generated_at = generated_at.to_string();
        for composition in &mut manifest.compositions {
            for managed_mod in &mut composition.mods {
                if managed_mod.slug == "nvidium" {
                    managed_mod.condition = croopor_performance::types::ModCondition::Always;
                    managed_mod.hardware_req = None;
                }
            }
        }
        manifest
    }

    fn signed_rules_response(manifest: &croopor_performance::Manifest) -> SignedRulesResponse {
        let signing_key = SigningKey::from_bytes(&[13_u8; 32]);
        let payload = croopor_performance::canonical_manifest_payload(manifest).expect("payload");
        let signature = signing_key.sign(&payload);
        SignedRulesResponse {
            public_key: hex::encode(signing_key.verifying_key().to_bytes()),
            signature: hex::encode(signature.to_bytes()),
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
            source: test_modrinth_source(),
            integrity: croopor_performance::ManagedArtifactIntegrity {
                sha512: String::new(),
                sha512_verified: false,
            },
        }
    }

    fn test_modrinth_source() -> croopor_performance::ManagedArtifactSource {
        croopor_performance::ManagedArtifactSource {
            provider: croopor_performance::ManagedArtifactProvider::Modrinth,
        }
    }

    fn valid_sha512() -> String {
        "a".repeat(128)
    }
}
