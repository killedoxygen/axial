use crate::application::{
    self, ContentCompatRequest, ContentCompatResponse, ContentInstallRequest, ContentPlanRequest,
    ContentSearchParams, ContentUpdatesResponse, InstallQueueStateResponse,
    InstanceContentResponse, ModpackFilesPlan, ModpackInstallRequest, ModpackTarget,
    ResolutionPlan, SearchHit,
};
use crate::state::AppState;
use axial_content::{ContentDetail, Page};
use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{delete, get, post},
};
use serde::Deserialize;

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/content/search", get(handle_search))
        .route("/api/v1/content/item", get(handle_detail))
        .route("/api/v1/content/plan", post(handle_plan))
        .route("/api/v1/content/install", post(handle_install))
        .route("/api/v1/content/compatibility", post(handle_compatibility))
        .route("/api/v1/content/modpack/target", get(handle_modpack_target))
        .route("/api/v1/content/modpack/files", get(handle_modpack_files))
        .route(
            "/api/v1/content/modpack/install",
            post(handle_modpack_install),
        )
        .route(
            "/api/v1/instances/{id}/content",
            get(handle_instance_content),
        )
        .route(
            "/api/v1/instances/{id}/content",
            delete(handle_instance_content_delete),
        )
        .route(
            "/api/v1/instances/{id}/content/updates",
            get(handle_instance_content_updates),
        )
        .route(
            "/api/v1/instances/{id}/content/uninstall",
            post(handle_instance_content_uninstalls),
        )
}

#[derive(Debug, Deserialize)]
struct CanonicalIdQuery {
    id: String,
}

#[derive(Debug, Deserialize)]
struct CanonicalIdsRequest {
    canonical_ids: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ModpackTargetQuery {
    id: String,
    #[serde(default)]
    version_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ModpackFilesQuery {
    instance_id: String,
    id: String,
    #[serde(default)]
    version_id: Option<String>,
}

async fn handle_search(
    State(state): State<AppState>,
    Query(params): Query<ContentSearchParams>,
) -> Result<Json<Page<SearchHit>>, (StatusCode, Json<serde_json::Value>)> {
    application::content_search(&state, params).await.map(Json)
}

async fn handle_detail(
    State(state): State<AppState>,
    Query(query): Query<CanonicalIdQuery>,
) -> Result<Json<ContentDetail>, (StatusCode, Json<serde_json::Value>)> {
    application::content_detail(&state, &query.id)
        .await
        .map(Json)
}

async fn handle_plan(
    State(state): State<AppState>,
    Json(payload): Json<ContentPlanRequest>,
) -> Result<Json<ResolutionPlan>, (StatusCode, Json<serde_json::Value>)> {
    application::content_plan(&state, payload).await.map(Json)
}

async fn handle_install(
    State(state): State<AppState>,
    Json(payload): Json<ContentInstallRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::queue_content_install(&state, payload)
        .await
        .map(Json)
}

async fn handle_compatibility(
    State(state): State<AppState>,
    Json(payload): Json<ContentCompatRequest>,
) -> Result<Json<ContentCompatResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::content_compatibility(&state, payload)
        .await
        .map(Json)
}

async fn handle_modpack_target(
    State(state): State<AppState>,
    Query(query): Query<ModpackTargetQuery>,
) -> Result<Json<ModpackTarget>, (StatusCode, Json<serde_json::Value>)> {
    application::modpack_target(&state, &query.id, query.version_id.as_deref())
        .await
        .map(Json)
}

async fn handle_modpack_files(
    State(state): State<AppState>,
    Query(query): Query<ModpackFilesQuery>,
) -> Result<Json<ModpackFilesPlan>, (StatusCode, Json<serde_json::Value>)> {
    application::modpack_files(
        &state,
        &query.instance_id,
        &query.id,
        query.version_id.as_deref(),
    )
    .await
    .map(Json)
}

async fn handle_modpack_install(
    State(state): State<AppState>,
    Json(payload): Json<ModpackInstallRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::queue_modpack_install(&state, payload)
        .await
        .map(Json)
}

async fn handle_instance_content(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceContentResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::instance_content(&state, &id).await.map(Json)
}

async fn handle_instance_content_updates(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<ContentUpdatesResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::instance_content_updates(&state, &id)
        .await
        .map(Json)
}

async fn handle_instance_content_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CanonicalIdQuery>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::queue_content_uninstall(&state, &id, &query.id)
        .await
        .map(Json)
}

async fn handle_instance_content_uninstalls(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<CanonicalIdsRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::queue_content_uninstalls(&state, &id, payload.canonical_ids)
        .await
        .map(Json)
}
