use super::error::ApiResult;
use axial_app::accounts::{session::AuthService, view::ProfileRefreshResponse};
use axum::{Json, Router, extract::State, routing::post};
use std::sync::Arc;

/// Commands on the selected Microsoft account. Mount beside the accounts router.
pub fn router(auth: Arc<AuthService>) -> Router {
    Router::new()
        .route("/api/v1/auth/refresh", post(refresh))
        .route("/api/v1/auth/profile/sync", post(sync))
        .with_state(auth)
}

async fn refresh(State(auth): State<Arc<AuthService>>) -> ApiResult<ProfileRefreshResponse> {
    let capture = auth.refresh_selected().await?;
    Ok(Json(ProfileRefreshResponse::refreshed(&capture)))
}

async fn sync(State(auth): State<Arc<AuthService>>) -> ApiResult<ProfileRefreshResponse> {
    let capture = auth.sync_selected_profile().await?;
    Ok(Json(ProfileRefreshResponse::synced(&capture)))
}
