use super::error::{ApiError, Extracted};
use axial_app::telemetry::{FrontendErrorReportRequest, Telemetry};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    routing::post,
};
use std::sync::Arc;

/// Mount inside the local API's authenticated, exact-origin transport boundary.
pub fn router(telemetry: Arc<Telemetry>) -> Router {
    Router::new()
        .route("/api/v1/telemetry/frontend-error", post(frontend_error))
        .layer(DefaultBodyLimit::max(4096))
        .with_state(telemetry)
}

async fn frontend_error(
    State(telemetry): State<Arc<Telemetry>>,
    request: Result<Json<FrontendErrorReportRequest>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let request = request.or_refuse(invalid_report)?;
    if !request.is_bounded() {
        return Err(invalid_report());
    }
    telemetry.report_frontend_error(request);
    Ok(StatusCode::NO_CONTENT)
}

fn invalid_report() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "invalid frontend error report")
}

#[cfg(test)]
mod tests {
    use crate::fixture::{Api, Profile};
    use reqwest::{Method, StatusCode};
    use serde_json::json;

    #[tokio::test]
    async fn malformed_unknown_and_oversized_reports_share_a_fixed_public_error() {
        let profile = Profile::start().await;
        let private = "/Users/private/token=canary";
        for body in [
            "{private malformed source".into(),
            json!({"kind": "token=private", "name": "Error", "message": "private"}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "private", "stack": private})
                .to_string(),
            json!({"kind": "render", "name": "n".repeat(65), "message": "private"}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "é".repeat(201)}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "private".repeat(1000)})
                .to_string(),
        ] {
            let request = profile
                .api
                .builder(Method::POST, "/api/v1/telemetry/frontend-error")
                .header("content-type", "application/json")
                .body(body);
            assert_eq!(
                Api::json(request).await,
                (
                    StatusCode::BAD_REQUEST,
                    json!({"error": "invalid frontend error report"})
                )
            );
        }
        let accepted = json!({"kind": "render", "name": private, "message": private});
        let response = profile
            .api
            .builder(Method::POST, "/api/v1/telemetry/frontend-error")
            .json(&accepted)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(response.bytes().await.unwrap().is_empty());
        profile.close().await;
    }
}
