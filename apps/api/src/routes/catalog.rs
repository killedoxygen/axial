use crate::application::{self, CatalogResponse};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    routing::get,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/catalog", get(handle_catalog))
}

async fn handle_catalog(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<CatalogResponse>, (StatusCode, Json<serde_json::Value>)> {
    let producer = handoff
        .try_claim()
        .map_err(super::producer_claim_error_response)?;
    application::catalog(&state, &producer).await.map(Json)
}
