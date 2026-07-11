use crate::application::{
    InstallQueueRequest, InstallQueueStateResponse, LoaderBuildsRequest, enqueue_install_owned,
    loader_builds, loader_components, loader_game_versions, loader_install_events_stream,
};
use crate::state::{AppState, RequestProducerHandoff};
use axial_minecraft::LoaderComponentId;
use axum::{
    Json, Router,
    extract::{Extension, Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct LoaderBuildQuery {
    mc_version: String,
}

#[derive(Debug, Deserialize)]
struct LoaderInstallRequest {
    component_id: String,
    build_id: String,
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/loaders/components", get(handle_loader_components))
        .route(
            "/api/v1/loaders/components/{id}/builds",
            get(handle_loader_builds),
        )
        .route(
            "/api/v1/loaders/components/{id}/game-versions",
            get(handle_loader_game_versions),
        )
        .route("/api/v1/loaders/install", post(handle_loader_install))
        .route(
            "/api/v1/loaders/install/{id}/events",
            get(handle_loader_install_events),
        )
}

async fn handle_loader_components() -> Json<crate::dto::loaders::LoaderComponentsResponse> {
    Json(loader_components())
}

async fn handle_loader_builds(
    Path(component_id): Path<String>,
    Query(query): Query<LoaderBuildQuery>,
    State(state): State<AppState>,
) -> Result<Json<crate::dto::loaders::LoaderBuildsResponse>, (StatusCode, Json<serde_json::Value>)>
{
    let component_id = parse_component_id(&component_id)?;
    loader_builds(
        &state,
        LoaderBuildsRequest {
            component_id,
            mc_version: query.mc_version,
        },
    )
    .await
    .map(Json)
}

async fn handle_loader_game_versions(
    Path(component_id): Path<String>,
    State(state): State<AppState>,
) -> Result<
    Json<crate::dto::loaders::LoaderGameVersionsResponse>,
    (StatusCode, Json<serde_json::Value>),
> {
    let component_id = parse_component_id(&component_id)?;
    loader_game_versions(&state, component_id).await.map(Json)
}

async fn handle_loader_install(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Json(payload): Json<LoaderInstallRequest>,
) -> Result<Json<InstallQueueStateResponse>, (StatusCode, Json<serde_json::Value>)> {
    enqueue_install_owned(
        &state,
        InstallQueueRequest {
            kind: "loader".to_string(),
            version_id: String::new(),
            manifest_url: String::new(),
            component_id: payload.component_id,
            build_id: payload.build_id,
        },
        handoff,
    )
    .await
    .map(Json)
}

async fn handle_loader_install_events(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    loader_install_events_stream(&state, &id).await
}

fn parse_component_id(
    component_id: &str,
) -> Result<LoaderComponentId, (StatusCode, Json<serde_json::Value>)> {
    LoaderComponentId::parse(component_id).ok_or_else(|| {
        (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({
                "error": "unknown loader component"
            })),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::parse_component_id;
    use axum::Json;
    use serde_json::json;

    #[test]
    fn parse_component_id_error_does_not_echo_raw_component() {
        let (status, Json(body)) =
            parse_component_id(r"C:\Users\Alice\.minecraft --accessToken raw-secret")
                .expect_err("invalid component should fail");

        assert_eq!(status, axum::http::StatusCode::NOT_FOUND);
        assert_eq!(body["error"], json!("unknown loader component"));
        assert!(!body.to_string().contains(r"C:\Users"));
        assert!(!body.to_string().contains("raw-secret"));
    }
}
