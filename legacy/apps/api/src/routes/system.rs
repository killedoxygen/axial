use crate::{application, state::AppState};
use axum::{Json, Router, extract::State, routing::get};

pub fn router() -> Router<AppState> {
    Router::new().route("/api/v1/system", get(handle_system))
}

async fn handle_system(
    State(_state): State<AppState>,
) -> Json<application::SystemResourceResponse> {
    Json(application::system_resource_status())
}
