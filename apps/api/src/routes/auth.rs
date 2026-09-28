use super::accounts::auth_error;
use axial_app::accounts::{session::AuthService, view::AuthStatusResponse};
use axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    routing::{get, post},
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone)]
struct AuthRoutes {
    auth: Arc<AuthService>,
    login_available: bool,
}

/// The native composition may set login_available only after its OAuth window
/// commands are registered. Browser composition always supplies false.
pub fn router(auth: Arc<AuthService>, login_available: bool) -> Router {
    Router::new()
        .route("/api/v1/auth/status", get(status))
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/profile/sync", post(sync))
        .route("/api/v1/auth/logout", post(logout))
        .with_state(AuthRoutes {
            auth,
            login_available,
        })
}

type ApiError = (StatusCode, Json<Value>);

async fn status(State(state): State<AuthRoutes>) -> Result<Json<AuthStatusResponse>, ApiError> {
    state
        .auth
        .status(state.login_available)
        .await
        .map(Json)
        .map_err(auth_error)
}

async fn refresh(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let capture = state.auth.refresh_selected().await.map_err(auth_error)?;
    Ok(Json(
        json!({"status":"refreshed", "account_id":capture.account_id(),
        "selection_revision":capture.selection_revision(), "minecraft_profile_ready":true,
        "minecraft_ownership_verified":true, "minecraft_profile":capture.profile(),
        "view_model":{"summary":"Microsoft sign-in refreshed."}}),
    ))
}

async fn sync(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let capture = state
        .auth
        .sync_selected_profile()
        .await
        .map_err(auth_error)?;
    Ok(Json(
        json!({"status":"profile_synced", "account_id":capture.account_id(),
        "selection_revision":capture.selection_revision(), "minecraft_profile_ready":true,
        "minecraft_ownership_verified":true, "minecraft_profile":capture.profile(),
        "view_model":{"summary":"Minecraft profile synced."}}),
    ))
}

async fn logout(State(state): State<AuthRoutes>) -> Result<Json<Value>, ApiError> {
    let snapshot = state.auth.logout().await.map_err(auth_error)?;
    Ok(Json(
        json!({"status":"logged_out", "selection_revision":snapshot.selection_revision,
        "view_model":{"summary":"Signed out of Microsoft."}}),
    ))
}
