//! Native package authority stays with the desktop adapter; HTTP carries only
//! commands and the feature-owned update state.
use axial_app::update::{UpdateError, UpdateFlow, UpdateInfo, UpdateService, UpdateSnapshot};
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State, rejection::JsonRejection},
    http::StatusCode,
    routing::{get, post},
};
use serde::Deserialize;
use serde_json::{Value, json};

type ApiError = (StatusCode, Json<Value>);

pub fn router(updates: UpdateService) -> Router {
    Router::new()
        .route("/api/v1/update", get(check))
        .route("/api/v1/update/flow", get(flow))
        .route("/api/v1/update/snapshot", get(snapshot))
        .route("/api/v1/update/download", post(download))
        .route("/api/v1/update/apply", post(apply))
        .layer(DefaultBodyLimit::max(1024))
        .with_state(updates)
}

async fn check(State(updates): State<UpdateService>) -> Result<Json<UpdateInfo>, ApiError> {
    updates.check().await.map(Json).map_err(update_error)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DownloadRequest {
    version: String,
}

async fn download(
    State(updates): State<UpdateService>,
    request: Result<Json<DownloadRequest>, JsonRejection>,
) -> Result<Json<UpdateFlow>, ApiError> {
    if !updates.snapshot().flow.supported {
        return Err(update_error(UpdateError::Unsupported));
    }
    let Json(request) = request.map_err(|error| {
        invalid_request(if error.status() == StatusCode::PAYLOAD_TOO_LARGE {
            StatusCode::PAYLOAD_TOO_LARGE
        } else {
            StatusCode::BAD_REQUEST
        })
    })?;
    if request.version.is_empty()
        || request.version.len() > 128
        || request.version.chars().any(char::is_control)
    {
        return Err(invalid_request(StatusCode::BAD_REQUEST));
    }
    updates
        .download(&request.version)
        .await
        .map(Json)
        .map_err(update_error)
}

async fn apply(State(updates): State<UpdateService>) -> Result<Json<UpdateFlow>, ApiError> {
    updates.apply().await.map(Json).map_err(update_error)
}

async fn flow(State(updates): State<UpdateService>) -> Json<UpdateFlow> {
    Json(updates.snapshot().flow)
}

async fn snapshot(State(updates): State<UpdateService>) -> Json<UpdateSnapshot> {
    Json(updates.snapshot())
}

fn invalid_request(status: StatusCode) -> ApiError {
    (
        status,
        Json(json!({
            "error": "Provide an update version in a valid download request.",
            "code": "invalid_update_request"
        })),
    )
}

fn update_error(error: UpdateError) -> ApiError {
    let (status, code) = match &error {
        UpdateError::Unsupported => (StatusCode::NOT_IMPLEMENTED, "update_unsupported"),
        UpdateError::Busy => (StatusCode::CONFLICT, "update_busy"),
        UpdateError::StaleRelease => (StatusCode::CONFLICT, "update_stale_release"),
        UpdateError::NotReady => (StatusCode::CONFLICT, "update_not_ready"),
        UpdateError::Failed(_) => (StatusCode::BAD_GATEWAY, "update_failed"),
    };
    let message = if matches!(error, UpdateError::Unsupported) {
        "In-app updates are unavailable for this build because a trusted release feed has not been configured.".into()
    } else {
        error.to_string().chars().take(500).collect::<String>()
    };
    (status, Json(json!({ "error": message, "code": code })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axial_app::update::{NativeUpdateAdapter, UpdateFuture, UpdatePhase};
    use axum::{body::Body, http::Request};
    use http_body_util::BodyExt;
    use std::sync::Arc;
    use tower::ServiceExt;

    struct FixtureAdapter {
        complete_apply: bool,
        check_error: Option<UpdateError>,
    }

    impl NativeUpdateAdapter for FixtureAdapter {
        fn check(&self, service: UpdateService) -> UpdateFuture<UpdateInfo> {
            let error = self.check_error.clone();
            Box::pin(async move {
                if let Some(error) = error {
                    return Err(error);
                }
                let attempt = service.begin_check()?;
                service.checked(
                    attempt,
                    Some(("1.1.0", "https://example.com/releases/1.1.0")),
                );
                Ok(service.snapshot().info.unwrap())
            })
        }

        fn download(&self, service: UpdateService, version: String) -> UpdateFuture<UpdateFlow> {
            Box::pin(async move {
                let attempt = service.begin_download(&version)?;
                service.progress(attempt, 100, Some(100));
                service.verifying(attempt);
                service.staged(attempt);
                Ok(service.snapshot().flow)
            })
        }

        fn apply(&self, service: UpdateService) -> UpdateFuture<UpdateFlow> {
            let complete = self.complete_apply;
            Box::pin(async move {
                let attempt = service.begin_apply()?;
                if complete {
                    service.installed(attempt);
                }
                Ok(service.snapshot().flow)
            })
        }
    }

    fn fixture(complete_apply: bool) -> UpdateService {
        let updates = UpdateService::new("1.0.0", "linux", "x86_64");
        updates
            .attach_adapter(Arc::new(FixtureAdapter {
                complete_apply,
                check_error: None,
            }))
            .unwrap();
        updates
    }

    async fn request(
        updates: &UpdateService,
        method: &str,
        path: &str,
        body: Option<&str>,
    ) -> (StatusCode, Value) {
        let mut request = Request::builder().method(method).uri(path);
        if body.is_some() {
            request = request.header("content-type", "application/json");
        }
        let response = router(updates.clone())
            .oneshot(
                request
                    .body(Body::from(body.unwrap_or("").to_owned()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[tokio::test]
    async fn native_commands_reach_the_adapter_and_publish_authoritative_status() {
        for complete_apply in [false, true] {
            let updates = fixture(complete_apply);
            let (status, info) = request(&updates, "GET", "/api/v1/update?force=1", None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(info["latest_version"], "1.1.0");
            assert_eq!(info["available"], true);
            let (status, flow) = request(
                &updates,
                "POST",
                "/api/v1/update/download",
                Some(r#"{"version":"1.1.0"}"#),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(flow["phase"], "ready");
            assert_eq!(flow["can_apply"], true);
            let (status, applied) = request(&updates, "POST", "/api/v1/update/apply", None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(
                applied["phase"],
                if complete_apply {
                    "restart-pending"
                } else {
                    "applying"
                }
            );
            let (status, observed) = request(&updates, "GET", "/api/v1/update/flow", None).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(observed, applied);
        }
    }

    #[tokio::test]
    async fn version_and_operation_fences_are_reported_without_advancing_state() {
        let updates = fixture(false);
        let before = updates.snapshot();
        let (status, error) = request(&updates, "POST", "/api/v1/update/apply", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "update_not_ready");
        assert_eq!(updates.snapshot(), before);

        request(&updates, "GET", "/api/v1/update", None).await;
        let checked = updates.snapshot();
        let (status, error) = request(
            &updates,
            "POST",
            "/api/v1/update/download",
            Some(r#"{"version":"9.9.9"}"#),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "update_stale_release");
        assert_eq!(updates.snapshot(), checked);

        request(
            &updates,
            "POST",
            "/api/v1/update/download",
            Some(r#"{"version":"1.1.0"}"#),
        )
        .await;
        let ready = updates.snapshot();
        assert_eq!(ready.flow.phase, UpdatePhase::Ready);
        let (status, error) = request(&updates, "GET", "/api/v1/update?force=1", None).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(error["code"], "update_busy");
        assert_eq!(updates.snapshot(), ready);
    }

    #[tokio::test]
    async fn download_requires_a_bounded_strict_version_body() {
        let updates = fixture(false);
        request(&updates, "GET", "/api/v1/update", None).await;
        let before = updates.snapshot();
        for body in [
            None,
            Some("{"),
            Some("{}"),
            Some(r#"{"version":42}"#),
            Some(r#"{"version":""}"#),
            Some(r#"{"version":"1.1.0\n"}"#),
            Some(r#"{"version":"1.1.0","path":"/tmp/package"}"#),
        ] {
            let (status, error) = request(&updates, "POST", "/api/v1/update/download", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "body: {body:?}");
            assert_eq!(error["code"], "invalid_update_request");
            assert_eq!(updates.snapshot(), before);
        }
        for (size, expected) in [
            (129, StatusCode::BAD_REQUEST),
            (2048, StatusCode::PAYLOAD_TOO_LARGE),
        ] {
            let body = json!({ "version": "v".repeat(size) }).to_string();
            let (status, error) =
                request(&updates, "POST", "/api/v1/update/download", Some(&body)).await;
            assert_eq!(status, expected);
            assert_eq!(error["code"], "invalid_update_request");
            assert_eq!(updates.snapshot(), before);
        }
    }

    #[tokio::test]
    async fn native_failures_have_a_bounded_error_envelope() {
        let updates = UpdateService::new("1.0.0", "linux", "x86_64");
        updates
            .attach_adapter(Arc::new(FixtureAdapter {
                complete_apply: false,
                check_error: Some(UpdateError::Failed("x".repeat(800))),
            }))
            .unwrap();
        let (status, error) = request(&updates, "GET", "/api/v1/update", None).await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        assert_eq!(error["code"], "update_failed");
        assert_eq!(error["error"].as_str().unwrap().len(), 500);
    }

    #[tokio::test]
    async fn unsupported_check_download_and_apply_never_publish_success() {
        let updates = UpdateService::new("1.0.0", "macos", "aarch64");
        let before = updates.snapshot();
        for (method, path) in [
            ("GET", "/api/v1/update?force=1"),
            ("POST", "/api/v1/update/download"),
            ("POST", "/api/v1/update/apply"),
        ] {
            let response = router(updates.clone())
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
            let body = response.into_body().collect().await.unwrap().to_bytes();
            let value: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(value["code"], "update_unsupported");
            assert!(
                value["error"]
                    .as_str()
                    .unwrap()
                    .contains("trusted release feed")
            );
            assert_eq!(updates.snapshot(), before);
        }
    }

    #[tokio::test]
    async fn status_reports_unsupported_without_inventing_a_release() {
        let updates = UpdateService::new("1.0.0", "browser", "unknown");
        let response = router(updates)
            .oneshot(
                Request::builder()
                    .uri("/api/v1/update/snapshot")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.into_body().collect().await.unwrap().to_bytes();
        let snapshot: UpdateSnapshot = serde_json::from_slice(&body).unwrap();
        assert_eq!(snapshot.info, None);
        assert!(!snapshot.flow.supported);
        assert!(!snapshot.flow.can_check);
        assert!(!snapshot.flow.can_download);
        assert!(!snapshot.flow.can_apply);
    }
}
