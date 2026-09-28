use axial_app::telemetry::{FrontendErrorReportRequest, Telemetry};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    routing::post,
};
use serde_json::{Value, json};
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
) -> Result<StatusCode, (StatusCode, Json<Value>)> {
    // Axum's rejection detail can contain untrusted field values. Return a fixed
    // envelope rather than reflecting source errors back into public output.
    let Json(request) = request.map_err(|_| invalid_report())?;
    if !request.is_bounded() {
        return Err(invalid_report());
    }
    telemetry.report_frontend_error(request);
    Ok(StatusCode::NO_CONTENT)
}

fn invalid_report() -> (StatusCode, Json<Value>) {
    (
        StatusCode::BAD_REQUEST,
        Json(json!({ "error": "invalid frontend error report" })),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::telemetry::{CollectorConfig, TelemetryEnvironment};
    use axum::{
        body::{Body, to_bytes},
        http::Request,
    };
    use tokio::{
        net::TcpListener,
        sync::mpsc,
        time::{Duration, timeout},
    };
    use tower::ServiceExt;

    fn request(body: String) -> Request<Body> {
        Request::builder()
            .method("POST")
            .uri("/api/v1/telemetry/frontend-error")
            .header("content-type", "application/json")
            .body(Body::from(body))
            .unwrap()
    }

    #[tokio::test]
    async fn malformed_unknown_and_oversized_reports_share_a_fixed_public_error() {
        let app = router(Arc::new(Telemetry::new(None)));
        for body in [
            "{private malformed source".into(),
            json!({"kind": "token=private", "name": "Error", "message": "private"}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "private", "stack": "/Users/private"}).to_string(),
            json!({"kind": "render", "name": "n".repeat(65), "message": "private"}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "é".repeat(201)}).to_string(),
            json!({"kind": "render", "name": "Error", "message": "private".repeat(1000)}).to_string(),
        ] {
            let response = app.clone().oneshot(request(body)).await.unwrap();
            assert_eq!(response.status(), StatusCode::BAD_REQUEST);
            let body = to_bytes(response.into_body(), 4096).await.unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&body).unwrap(), json!({"error": "invalid frontend error report"}));
        }
    }

    #[tokio::test]
    async fn route_discards_source_text_and_respects_backend_consent() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let host = format!("http://{}", listener.local_addr().unwrap());
        let (captured, mut requests) = mpsc::unbounded_channel::<Value>();
        let collector = Router::new()
            .route(
                "/batch/",
                post(
                    |State(captured): State<mpsc::UnboundedSender<Value>>,
                     Json(body): Json<Value>| async move {
                        captured.send(body).unwrap();
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(captured);
        let server = tokio::spawn(async move {
            axum::serve(listener, collector).await.unwrap();
        });
        let telemetry = Arc::new(Telemetry::new(Some(
            CollectorConfig::new("phc_fixture", &host, TelemetryEnvironment::Test).unwrap(),
        )));
        let app = router(telemetry.clone());
        let private = "/Users/private/token=canary account@example.com";
        let body = json!({"kind": "render", "name": private, "message": private}).to_string();
        let response = app.clone().oneshot(request(body.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(telemetry.flush_once().await, 0);
        assert!(requests.try_recv().is_err());

        telemetry
            .consent_change()
            .await
            .publish(true, Some("4d8fa83c-5815-4ea2-aac1-ddcc336c405e"));
        let response = app.clone().oneshot(request(body.clone())).await.unwrap();
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert!(
            to_bytes(response.into_body(), 4096)
                .await
                .unwrap()
                .is_empty()
        );
        assert_eq!(telemetry.flush_once().await, 1);
        let sent = timeout(Duration::from_secs(1), requests.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            sent["batch"][0]["properties"]["$exception_fingerprint"],
            "frontend_error"
        );
        assert_eq!(
            sent["batch"][0]["properties"]["$exception_list"],
            json!([{"type": "frontend_error", "value": "Frontend error occurred."}])
        );
        assert!(!sent.to_string().contains(private));

        telemetry.consent_change().await.publish(false, None);
        assert_eq!(
            app.oneshot(request(body)).await.unwrap().status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(telemetry.flush_once().await, 0);
        assert!(requests.try_recv().is_err());
        server.abort();
        assert!(server.await.unwrap_err().is_cancelled());
    }
}
