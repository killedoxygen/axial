use crate::application::{self, VersionsResponse};
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, State},
    http::StatusCode,
    routing::get,
};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/versions", get(handle_versions))
}

async fn handle_versions(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<VersionsResponse>, (StatusCode, Json<serde_json::Value>)> {
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(super::producer_claim_error_response)?;
    application::installed_versions(&state, &producer)
        .await
        .map(Json)
}
