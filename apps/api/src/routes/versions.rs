use super::error::{ApiError, ApiResult};
use axial_app::catalog::{CatalogService, VersionsResponse};
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

pub fn router(catalog: CatalogService) -> Router {
    Router::new()
        .route("/api/v1/versions", get(installed))
        .with_state(catalog)
}

async fn installed(State(catalog): State<CatalogService>) -> ApiResult<VersionsResponse> {
    catalog
        .installed()
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
