use crate::application::{
    self, PerformanceHealthRequest, PerformanceHealthResponse, PerformanceInstallRequest,
    PerformanceInstallResponse, PerformanceInstanceOperationResponse, PerformancePlanRequest,
    PerformancePlanResponse, PerformanceRollbackListRequest, PerformanceRollbackListResponse,
};
use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

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

pub(crate) fn spawn_pending_performance_operations(state: &AppState) -> bool {
    application::spawn_pending_performance_operations(state)
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
    application::refresh_performance_rules(&state)
        .await
        .map(Json)
        .map_err(application::refresh_performance_rules_error_response)
}

async fn handle_plan(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<PerformancePlanResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_plan(
        &state,
        PerformancePlanRequest {
            game_version: query.game_version,
            loader: query.loader,
            mode: query.mode,
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<PerformanceHealthResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_health(
        &state,
        PerformanceHealthRequest {
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_rollback_list(
    State(state): State<AppState>,
    Query(query): Query<RollbackQuery>,
) -> Result<Json<PerformanceRollbackListResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_rollback_list(
        &state,
        PerformanceRollbackListRequest {
            instance_id: query.instance_id,
        },
    )
    .await
    .map(Json)
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<Json<PerformanceInstallResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_install(
        state,
        PerformanceInstallRequest {
            instance_id: payload.instance_id,
            game_version: payload.game_version,
            loader: payload.loader,
            mode: payload.mode,
            action: payload.action,
            rollback_id: payload.rollback_id,
            queued: payload.queued,
        },
    )
    .await
    .map(Json)
}

async fn handle_operation_status(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<
    Json<crate::state::performance_operations::PerformanceOperationStatus>,
    (StatusCode, Json<serde_json::Value>),
> {
    application::performance_operation_status(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_operation(
    State(state): State<AppState>,
    Path(instance_id): Path<String>,
) -> Result<Json<PerformanceInstanceOperationResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::performance_instance_operation(&state, &instance_id)
        .await
        .map(Json)
}
