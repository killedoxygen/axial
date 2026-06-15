use crate::{
    application::{self, AuthStatusResponse},
    state::AppState,
};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/api/v1/auth/status", get(handle_auth_status))
        .route("/api/v1/auth/refresh", post(handle_auth_refresh))
        .route("/api/v1/auth/profile/sync", post(handle_auth_profile_sync))
        .route("/api/v1/auth/logout", post(handle_auth_logout))
}

async fn handle_auth_status(
    State(state): State<AppState>,
) -> Result<Json<AuthStatusResponse>, (StatusCode, Json<serde_json::Value>)> {
    application::auth_status(&state).await
}

async fn handle_auth_refresh(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_refresh_for_state(&state).await
}

async fn handle_auth_profile_sync(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_profile_sync_for_state(&state).await
}

async fn handle_auth_logout(
    State(state): State<AppState>,
) -> (StatusCode, Json<serde_json::Value>) {
    application::auth_logout_for_state(&state).await
}
