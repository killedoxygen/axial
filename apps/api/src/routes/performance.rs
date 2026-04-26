use crate::state::AppState;
use axum::{
    Json, Router,
    extract::{Query, State},
    http::StatusCode,
    routing::{get, post},
};
use croopor_performance::{
    BundleHealth, PerformanceMode, ResolutionRequest, derive_health, extract_base_version,
    infer_loader_from_version_id, load_state, parse_mode,
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct PlanQuery {
    game_version: String,
    #[allow(dead_code)]
    loader: Option<String>,
    #[allow(dead_code)]
    mode: Option<String>,
}

#[derive(Debug, Deserialize)]
struct HealthQuery {
    instance_id: String,
}

#[derive(Debug, Deserialize)]
struct InstallRequest {
    instance_id: String,
    #[allow(dead_code)]
    game_version: Option<String>,
    #[allow(dead_code)]
    loader: Option<String>,
    #[allow(dead_code)]
    mode: Option<String>,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/performance/plan", get(handle_plan))
        .route("/api/v1/performance/health", get(handle_health))
        .route("/api/v1/performance/install", post(handle_install))
}

async fn handle_plan(
    State(state): State<AppState>,
    Query(query): Query<PlanQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if query.game_version.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "game_version query parameter is required" })),
        ));
    }

    let mode = resolve_config_mode(&state, query.mode.as_deref())?;
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: query.game_version.trim().to_string(),
        loader: query.loader.unwrap_or_default(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: Vec::new(),
    });

    Ok(Json(serde_json::to_value(plan).unwrap_or_else(
        |_| serde_json::json!({ "active": false }),
    )))
}

async fn handle_health(
    State(state): State<AppState>,
    Query(query): Query<HealthQuery>,
) -> Result<Json<serde_json::Value>, (StatusCode, Json<serde_json::Value>)> {
    if query.instance_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "instance_id query parameter is required" })),
        ));
    }
    if state.instances().get(&query.instance_id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        ));
    }

    let instance = state.instances().get(&query.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");
    let state_file = load_state(&mods_dir).map_err(internal_error)?;
    let mode = resolve_instance_mode(&state, &instance.performance_mode, None)?;

    if !matches!(mode, PerformanceMode::Managed) {
        return Ok(Json(serde_json::json!({
            "active": true,
            "health": BundleHealth::Disabled,
            "composition_id": "",
            "tier": "",
            "installed_count": 0,
            "warnings": [],
        })));
    }

    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: extract_base_version(&instance.version_id),
        loader: infer_loader_from_version_id(&instance.version_id),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: installed_mod_ids_from_state(state_file.as_ref()),
    });
    let (health, warnings) = derive_health(state_file.as_ref(), Some(&plan), &mods_dir);

    Ok(Json(serde_json::json!({
        "active": true,
        "health": health,
        "composition_id": state_file.as_ref().map(|value| value.composition_id.clone()).unwrap_or_default(),
        "tier": state_file.as_ref().map(|value| value.tier).map(|value| serde_json::to_value(value).unwrap_or(serde_json::Value::Null)).unwrap_or(serde_json::Value::String(String::new())),
        "installed_count": state_file.as_ref().map(|value| value.installed_mods.len()).unwrap_or_default(),
        "warnings": [],
        "warnings": warnings,
    })))
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<InstallRequest>,
) -> Result<(StatusCode, Json<serde_json::Value>), (StatusCode, Json<serde_json::Value>)> {
    if payload.instance_id.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": "instance_id is required" })),
        ));
    }
    if state.instances().get(&payload.instance_id).is_none() {
        return Err((
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        ));
    }

    let instance = state.instances().get(&payload.instance_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": "instance not found" })),
        )
    })?;
    let mode = resolve_instance_mode(&state, &instance.performance_mode, payload.mode.as_deref())?;
    let game_version = payload
        .game_version
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| extract_base_version(&instance.version_id));
    let loader = payload
        .loader
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| infer_loader_from_version_id(&instance.version_id));
    let plan = state.performance().get_plan(ResolutionRequest {
        game_version: game_version.clone(),
        loader: loader.clone(),
        mode,
        hardware: state.performance().hardware(),
        installed_mods: Vec::new(),
    });
    let performance = state.performance().clone();
    let mods_dir = state.instances().game_dir(&instance.id).join("mods");

    tokio::spawn(async move {
        if matches!(mode, PerformanceMode::Managed) {
            let _ = performance
                .ensure_installed(&plan, &game_version, &mods_dir)
                .await;
        } else {
            let _ = performance.remove_managed(&mods_dir);
        }
    });

    Ok((
        StatusCode::ACCEPTED,
        Json(serde_json::json!({
            "active": true,
            "status": "installing",
        })),
    ))
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
