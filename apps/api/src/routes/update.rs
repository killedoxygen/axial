use crate::application::{self, UpdateResponse};
use crate::state::AppState;
use axum::{Json, Router, extract::State, http::StatusCode, routing::get};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/update", get(handle_update))
}

async fn handle_update(
    State(state): State<AppState>,
) -> Result<Json<UpdateResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::update_status(&state).await.map(Json)
}
