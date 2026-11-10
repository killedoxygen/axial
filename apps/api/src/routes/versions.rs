use super::error::{ApiError, ApiResult};
use axial_app::catalog::{CatalogService, CatalogSnapshot, VersionsResponse};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

pub fn router(catalog: CatalogService) -> Router {
    Router::new()
        .route("/api/v1/versions", get(installed))
        .route("/api/v1/versions/catalog", get(snapshot))
        .with_state(catalog)
}

async fn installed(State(catalog): State<CatalogService>) -> ApiResult<VersionsResponse> {
    catalog
        .installed()
        .await
        .map(Json)
        .map_err(|_| unavailable())
}

async fn snapshot(State(catalog): State<CatalogService>) -> ApiResult<CatalogSnapshot> {
    catalog
        .snapshot()
        .await
        .map(Json)
        .map_err(|_| unavailable())
}

fn unavailable() -> ApiError {
    ApiError::new(
        StatusCode::SERVICE_UNAVAILABLE,
        "Minecraft versions are unavailable.",
    )
}
