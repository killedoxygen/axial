use crate::application::{self, CatalogResponse};
use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/catalog", get(handle_catalog))
}

async fn handle_catalog(
    State(state): State<AppState>,
) -> Result<Json<CatalogResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::catalog(&state).await.map(Json)
}
