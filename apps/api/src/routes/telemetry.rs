use crate::{
    application::{self, FrontendErrorReportRequest},
    state::AppState,
};
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};

pub fn router() -> Router<AppState> {
    Router::new().route(
        "/api/v1/telemetry/frontend-error",
        post(handle_frontend_error),
    )
}

async fn handle_frontend_error(
    State(state): State<AppState>,
    Json(request): Json<FrontendErrorReportRequest>,
) -> Result<StatusCode, (StatusCode, Json<serde_json::Value>)> {
    application::report_frontend_error(&state, request)?;
    Ok(StatusCode::NO_CONTENT)
}
