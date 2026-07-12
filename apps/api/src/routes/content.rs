use crate::application::{
    self, ContentPlanRequest, ContentSearchParams, InstanceContentResponse, ResolutionPlan,
};
use crate::state::AppState;
use axial_content::{CanonicalContent, ContentDetail, Page};
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
        .route(
            "/api/v1/instances/{id}/content",
            get(handle_instance_content),
        )
        .route(
            "/api/v1/instances/{id}/content",
            delete(handle_instance_content_delete),
        )
}

#[derive(Debug, Deserialize)]
struct CanonicalIdQuery {
    id: String,
}

async fn handle_search(
    State(state): State<AppState>,
    Query(params): Query<ContentSearchParams>,
) -> Result<Json<Page<CanonicalContent>>, (StatusCode, Json<serde_json::Value>)> {
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
    Json(payload): Json<ContentPlanRequest>,
) -> Result<Json<InstanceContentResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::content_install(&state, payload)
        .await
        .map(Json)
}

async fn handle_instance_content(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<Json<InstanceContentResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::instance_content(&state, &id).await.map(Json)
}

async fn handle_instance_content_delete(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Query(query): Query<CanonicalIdQuery>,
) -> Result<Json<InstanceContentResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::content_uninstall(&state, &id, &query.id)
        .await
        .map(Json)
}
