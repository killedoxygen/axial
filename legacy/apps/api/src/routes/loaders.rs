use crate::application::loader_install_events_stream;
use crate::state::{AppState, RequestProducerHandoff};
use axum::{
    Json, Router,
    extract::{Extension, Path, State},
    http::StatusCode,
    routing::get,
};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/loaders/install/{id}/events",
        get(handle_loader_install_events),
    )
}

async fn handle_loader_install_events(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
    Path(id): Path<String>,
) -> Result<impl axum::response::IntoResponse, (StatusCode, Json<serde_json::Value>)> {
    let producer = state
        .try_claim_request_producer(&handoff)
        .map_err(super::producer_claim_error_response)?;
    loader_install_events_stream(&state, &id, producer).await
}
