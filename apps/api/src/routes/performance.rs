//! Authenticated HTTP adaptation for the Performance owner.

use axial_app::{
    instances::model::InstanceId,
    performance::{
        PerformanceMutationError, PerformanceService,
        health::health_response,
        model::PerformancePlanRequest,
        plan::{configured_mode, plan_response, resolve_mode, version_target},
    },
    settings::SettingsStore,
};
use axum::{
    Json, Router,
    extract::{
        DefaultBodyLimit, Path, Query, State,
        rejection::{JsonRejection, QueryRejection},
    },
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct PerformanceApi {
    service: Arc<PerformanceService>,
    settings: Arc<SettingsStore>,
}
type ApiResult = Result<Json<Value>, (StatusCode, Json<Value>)>;

pub fn router(service: Arc<PerformanceService>, settings: Arc<SettingsStore>) -> Router {
    Router::new()
        .route("/api/v1/performance/status", get(status))
        .route("/api/v1/performance/rules/refresh", post(refresh))
        .route("/api/v1/performance/plan", get(plan))
        .route("/api/v1/performance/health", get(health))
        .route("/api/v1/performance/rollback", get(rollback))
        .route("/api/v1/performance/install", post(install))
        .route(
            "/api/v1/performance/instances/{id}/operation",
            get(instance_operation),
        )
        .route("/api/v1/performance/operations/{id}", get(operation))
        .layer(DefaultBodyLimit::max(8192))
        .with_state(PerformanceApi { service, settings })
}

async fn status(State(api): State<PerformanceApi>) -> ApiResult {
    Ok(Json(json!(api.service.rules().status())))
}
async fn refresh(State(api): State<PerformanceApi>) -> ApiResult {
    api.service
        .rules()
        .refresh()
        .await
        .map(|value| Json(json!(value)))
        .map_err(|error| {
            (
                StatusCode::CONFLICT,
                Json(json!({"error":error.to_string()})),
            )
        })
}
async fn plan(
    State(api): State<PerformanceApi>,
    query: Result<Query<PerformancePlanRequest>, QueryRejection>,
) -> ApiResult {
    let Query(input) = query.map_err(|_| invalid())?;
    let request = resolution(&api, &input)?;
    let planned = api.service.rules().plan(request).await.map_err(|error| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error":error.to_string()})),
        )
    })?;
    Ok(Json(json!(plan_response(planned.plan()))))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstanceQuery {
    instance_id: Option<String>,
}
async fn health(
    State(api): State<PerformanceApi>,
    query: Result<Query<InstanceQuery>, QueryRejection>,
) -> ApiResult {
    let Query(query) = query.map_err(|_| invalid())?;
    let id = selected(&api, query.instance_id.as_deref())?;
    let request = resolution(
        &api,
        &PerformancePlanRequest {
            instance_id: Some(id.to_string()),
            ..Default::default()
        },
    )?;
    Ok(Json(json!(health_response(
        api.service
            .inspect_with_request(&id, Some(request))
            .await
            .map_err(error)?
    ))))
}
async fn rollback(
    State(api): State<PerformanceApi>,
    query: Result<Query<InstanceQuery>, QueryRejection>,
) -> ApiResult {
    let Query(query) = query.map_err(|_| invalid())?;
    let id = selected(&api, query.instance_id.as_deref())?;
    let inspected = api.service.inspect(&id).await.map_err(error)?;
    Ok(Json(json!({"snapshots":inspected.rollback_snapshots})))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct InstallRequest {
    instance_id: Option<String>,
    game_version: Option<String>,
    loader: Option<String>,
    mode: Option<String>,
    action: Option<String>,
    rollback_id: Option<String>,
    queued: Option<bool>,
}
async fn install(
    State(api): State<PerformanceApi>,
    body: Result<Json<InstallRequest>, JsonRejection>,
) -> ApiResult {
    let Json(body) = body.map_err(|_| invalid())?;
    let id = selected(&api, body.instance_id.as_deref())?;
    let request = resolution(
        &api,
        &PerformancePlanRequest {
            instance_id: Some(id.to_string()),
            game_version: body.game_version,
            loader: body.loader,
            mode: body.mode,
        },
    )?;
    let action = body.action.as_deref().unwrap_or("apply");
    if body.queued == Some(true) {
        let operation = api
            .service
            .submit(&id, request, action, body.rollback_id)
            .map_err(error)?;
        return Ok(Json(
            json!({"active":true,"status":"queued","install_id":operation.id,"health":"disabled","composition_id":"","tier":"","installed_count":0,"managed_artifacts":[],"warnings":[],"operation":operation_payload(operation)}),
        ));
    }
    let inspected = match action {
        "apply" | "reapply" => api.service.apply(&id, request).await,
        "remove" => api.service.remove(&id).await,
        "rollback" => api.service.rollback(&id, body.rollback_id).await,
        _ => return Err(invalid()),
    }
    .map_err(error)?;
    let state = inspected.state.as_ref();
    let response = json!({"active":state.is_some(),"status":"complete","health":inspected.health,
        "composition_id":state.map(|s| s.composition_id.as_str()).unwrap_or(""), "tier":state.map(|s| match s.tier { axial_app::performance::model::CompositionTier::Core => "core", axial_app::performance::model::CompositionTier::Extended => "extended", axial_app::performance::model::CompositionTier::VanillaEnhanced => "vanilla_enhanced" }).unwrap_or(""),
        "installed_count":state.map_or(0,|s|s.installed_mods.len()),"managed_artifacts":health_response(inspected.clone()).managed_artifacts,"warnings":inspected.warnings});
    Ok(Json(response))
}
async fn operation(State(api): State<PerformanceApi>, Path(id): Path<String>) -> ApiResult {
    let operation = api.service.operation(&id).map_err(error)?.ok_or((
        StatusCode::NOT_FOUND,
        Json(json!({"error":"performance operation not found"})),
    ))?;
    Ok(Json(operation_payload(operation)))
}
async fn instance_operation(
    State(api): State<PerformanceApi>,
    Path(id): Path<String>,
) -> ApiResult {
    let id = id.parse().map_err(|_| invalid())?;
    Ok(Json(
        json!({"operation":api.service.instance_operation(&id).map_err(error)?.map(operation_payload)}),
    ))
}
fn operation_payload(
    operation: axial_app::performance::mutation::PerformanceOperationStatus,
) -> Value {
    let complete = operation.state == "complete";
    let terminal = matches!(
        operation.state.as_str(),
        "complete" | "failed" | "interrupted"
    );
    let title = if operation.history.is_some() {
        "Historical Performance operation"
    } else {
        "Performance operation"
    };
    let mut value = json!(operation);
    value["view_model"] = json!({"state_label":operation.state,"tone":if complete {"ok"} else if terminal {"warn"} else {"mute"},
        "title":title,"detail":operation.error.unwrap_or_default(),"progress":{"phase":operation.state,"current":if terminal {1}else{0},"total":1,"done":terminal},"is_terminal":terminal,"is_complete":complete});
    value
}
fn resolution(
    api: &PerformanceApi,
    input: &PerformancePlanRequest,
) -> Result<axial_app::performance::model::ResolutionRequest, (StatusCode, Json<Value>)> {
    let settings = api.settings.current().map_err(|_| unavailable())?;
    let record = input
        .instance_id
        .as_deref()
        .map(|raw| {
            let id: InstanceId = raw.parse().map_err(|_| invalid())?;
            api.service
                .instances()
                .registry()
                .get_live(&id)
                .map_err(|_| unavailable())
        })
        .transpose()?;
    let mode = resolve_mode(
        configured_mode(settings.performance_mode),
        record
            .as_ref()
            .map(|r| r.instance.settings.performance_mode.as_str()),
        input.mode.as_deref(),
    )
    .map_err(|_| invalid())?;
    let (game, loader) = version_target(
        record
            .as_ref()
            .map(|r| r.instance.minecraft_version.as_str()),
        record.as_ref().map(|r| r.instance.loader_key.as_str()),
        input.game_version.as_deref(),
        input.loader.as_deref(),
    )
    .map_err(|_| invalid())?;
    Ok(api.service.resolution_request(game, loader, mode.mode))
}
fn selected(
    api: &PerformanceApi,
    raw: Option<&str>,
) -> Result<InstanceId, (StatusCode, Json<Value>)> {
    if let Some(raw) = raw {
        return raw.parse().map_err(|_| invalid());
    }
    api.service
        .instances()
        .registry()
        .last_instance_id()
        .map_err(|_| unavailable())?
        .ok_or_else(invalid)
}
fn error(error: PerformanceMutationError) -> (StatusCode, Json<Value>) {
    let code = match error {
        PerformanceMutationError::SnapshotNotFound => StatusCode::NOT_FOUND,
        PerformanceMutationError::Storage(_) => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::CONFLICT,
    };
    (code, Json(json!({"error":error.to_string()})))
}
fn invalid() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({"error":"invalid performance request"})),
    )
}
fn unavailable() -> (StatusCode, Json<Value>) {
    (
        StatusCode::CONFLICT,
        Json(json!({"error":"performance target is unavailable"})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_projection_preserves_history_and_unchanged_live_wire() {
        let mut record = json!({
            "id":"e89198a7-fc9a-4268-8237-1d03a7865d7d",
            "instance_id":"d6926227-f5e6-47a3-a736-f6c1cef14435", "action":"apply",
            "state":"failed", "error":"Operation failed",
            "created_at":"2026-01-01T00:00:00.000Z", "updated_at":"2026-01-01T00:00:01.000Z"
        });
        let live = operation_payload(serde_json::from_value(record.clone()).unwrap());
        assert!(live.get("history").is_none());
        assert_eq!(live["view_model"]["title"], "Performance operation");
        let history = json!({
            "operation_id":"op-2b709312-e6ce-4ee3-89c6-2d15d9b30d66", "sequence":3,
            "intent":{
                "instance_id":"0000000000000001", "requested_action":"install", "action":"remove",
                "base_target_id":"performance_composition_lock", "rollback":"Unavailable",
                "game_version":"1.20.1", "loader":"fabric", "mode":"custom"
            },
            "terminal":{"outcome":"failed_before_effect", "error":"Operation failed"}
        });
        record["id"] = json!(format!("legacy-performance-{:064x}", 3));
        record["action"] = json!("install");
        record["history"] = history.clone();
        let historical = operation_payload(serde_json::from_value(record).unwrap());
        assert_eq!(historical["history"], history);
        assert_eq!(historical["action"], "install");
        assert_eq!(
            historical["view_model"]["title"],
            "Historical Performance operation"
        );
        assert_eq!(historical["view_model"]["is_terminal"], true);
        assert_eq!(historical["view_model"]["is_complete"], false);
        assert_eq!(historical["view_model"]["progress"]["done"], true);
    }
}
