use crate::{
    application,
    state::{AppState, RequestProducerHandoff},
};
use axum::{Extension, Json, Router, extract::State, http::StatusCode, routing::post};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/setup/init", post(handle_setup_init))
        .route(
            "/api/v1/onboarding/complete",
            post(handle_onboarding_complete),
        )
}

async fn handle_setup_init(
    State(state): State<AppState>,
    Extension(handoff): Extension<RequestProducerHandoff>,
) -> Result<Json<application::SetupLibraryResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::setup_init_owned(&state, handoff)
        .await
        .map(Json)
}

async fn handle_onboarding_complete(
    State(state): State<AppState>,
) -> Result<Json<application::SetupStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::onboarding_complete(&state).await.map(Json)
}
