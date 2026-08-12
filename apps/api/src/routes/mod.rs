mod accounts;
mod auth;
mod catalog;
mod config;
mod content;
mod extract;
mod flags;
mod install;
mod instances;
mod java;
mod launch;
mod loaders;
mod music;
mod performance;
mod setup;
mod skin;
mod status;
mod system;
mod telemetry;
mod update;
mod version_info;
mod versions;

use extract::{ApiJson, ApiQuery};

use crate::state::{AppState, LifecycleAdmissionError, RequestLease};
use crate::transport::{CAPABILITY_HEADER, LocalApiAuthority};
use axum::{
    Json, Router,
    body::Body,
    extract::{Extension, Request, State},
    http::{HeaderName, Method, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{any, post},
};
use http_body_util::BodyExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

#[cfg(test)]
pub fn router(state: AppState) -> Router {
    router_with_authority(state, LocalApiAuthority::bypass_for_test())
}

pub(crate) fn router_with_authority(state: AppState, authority: LocalApiAuthority) -> Router {
    let admission_state = state.clone();
    let auth_state = authority.clone();
    Router::new()
        .merge(status::router())
        .merge(accounts::router())
        .merge(auth::router())
        .merge(system::router())
        .merge(telemetry::router())
        .merge(config::router())
        .merge(flags::router())
        .merge(setup::router())
        .merge(catalog::router())
        .merge(content::router())
        .merge(instances::router())
        .merge(install::router())
        .merge(music::router())
        .merge(performance::router())
        .merge(skin::router())
        .merge(update::router())
        .merge(launch::router())
        .merge(loaders::router())
        .merge(versions::router())
        .merge(version_info::router())
        .merge(java::router())
        .route(
            "/api/v1/transport/bootstrap",
            post(crate::transport::create_bootstrap),
        )
        .route(
            "/api/v1/transport/tickets",
            post(crate::transport::create_ticket),
        )
        .route("/api", any(api_not_found))
        .route("/api/{*path}", any(api_not_found))
        .method_not_allowed_fallback(api_method_not_allowed)
        .with_state(state)
        .layer(Extension(authority.clone()))
        .layer(middleware::from_fn_with_state(
            admission_state,
            lifecycle_admission,
        ))
        .layer(local_cors_layer(authority))
        .layer(middleware::from_fn_with_state(
            auth_state,
            crate::transport::authenticate_request,
        ))
}

async fn api_not_found() -> impl IntoResponse {
    (
        axum::http::StatusCode::NOT_FOUND,
        Json(serde_json::json!({ "error": "API route was not found" })),
    )
}

async fn api_method_not_allowed() -> impl IntoResponse {
    (
        axum::http::StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({ "error": "API method is not allowed" })),
    )
}

async fn lifecycle_admission(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Response {
    let lease = match state.try_admit_request() {
        Ok(lease) => lease,
        Err(error) => {
            return (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "error": error.to_string() })),
            )
                .into_response();
        }
    };
    request.extensions_mut().insert(lease.producer_handoff());
    hold_request_lease(next.run(request).await, lease)
}

fn hold_request_lease(mut response: Response, lease: RequestLease) -> Response {
    let body = std::mem::take(response.body_mut());
    *response.body_mut() = Body::new(body.map_frame(move |frame| {
        let _ = &lease;
        frame
    }));
    response
}

pub(super) fn producer_claim_error_response(
    _error: LifecycleAdmissionError,
) -> (axum::http::StatusCode, Json<serde_json::Value>) {
    (
        axum::http::StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "application shutdown is in progress" })),
    )
}

fn local_cors_layer(authority: LocalApiAuthority) -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(move |origin, _| {
            authority.allows_origin(origin)
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([
            header::CONTENT_TYPE,
            HeaderName::from_static(CAPABILITY_HEADER),
        ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{AppStateInit, InstallStore, RequestProducerHandoff, SessionStore};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use axum::body::{Body, Bytes, to_bytes};
    use axum::extract::Extension;
    use axum::routing::get;
    use std::convert::Infallible;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    use tokio::sync::Notify;
    use tower::ServiceExt;

    #[tokio::test]
    async fn p02_b03_contract_cross_owner_production_routes_share_bounded_rejections() {
        let fixture = TestFixture::new("bounded-extraction");
        let app = router(fixture.state.clone());
        let private = "private-request-token";
        let oversized = format!(
            "{{\"theme\":\"system\",\"padding\":\"{}\"}}",
            "x".repeat(2 * 1024 * 1024)
        );

        for (method, uri, content_type, body, status, error) in [
            (
                Method::PUT,
                "/api/v1/config",
                Some("application/json"),
                format!("{{\"theme\":\"obsidian\"}}{private}"),
                axum::http::StatusCode::BAD_REQUEST,
                "Invalid JSON syntax.",
            ),
            (
                Method::PUT,
                "/api/v1/config",
                Some("application/json"),
                format!("{{\"unknown\":\"{private}\"}}"),
                axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                "Invalid JSON request.",
            ),
            (
                Method::GET,
                "/api/v1/loaders/components/fabric/builds",
                None,
                String::new(),
                axum::http::StatusCode::BAD_REQUEST,
                "Invalid query request.",
            ),
            (
                Method::GET,
                "/api/v1/music/track?t=private-query-token",
                None,
                String::new(),
                axum::http::StatusCode::BAD_REQUEST,
                "Invalid query request.",
            ),
            (
                Method::PUT,
                "/api/v1/config",
                Some("application/json"),
                oversized,
                axum::http::StatusCode::PAYLOAD_TOO_LARGE,
                "JSON request is too large.",
            ),
        ] {
            let mut request = Request::builder().method(method).uri(uri);
            if let Some(content_type) = content_type {
                request = request.header(header::CONTENT_TYPE, content_type);
            }
            let response = app
                .clone()
                .oneshot(request.body(Body::from(body)).expect("bounded request"))
                .await
                .expect("bounded extraction response");
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_TYPE)
                    .and_then(|value| value.to_str().ok()),
                Some("application/json"),
                "{uri}"
            );
            let bytes = to_bytes(response.into_body(), 1024)
                .await
                .expect("bounded extraction body");
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .expect("bounded extraction JSON"),
                serde_json::json!({ "error": error }),
                "{uri}"
            );
            assert!(!String::from_utf8_lossy(&bytes).contains(private), "{uri}");
        }
    }

    #[tokio::test]
    async fn p02_b06_contract_api_fallthrough_is_json_and_method_specific() {
        let fixture = TestFixture::new("api-routing-contract");
        let app = router(fixture.state.clone());

        for (method, uri, status, error) in [
            (
                Method::GET,
                "/api/v1/not-a-real-route",
                axum::http::StatusCode::NOT_FOUND,
                "API route was not found",
            ),
            (
                Method::POST,
                "/api/v1/not-a-real-route",
                axum::http::StatusCode::NOT_FOUND,
                "API route was not found",
            ),
            (
                Method::GET,
                "/api/v2/status",
                axum::http::StatusCode::NOT_FOUND,
                "API route was not found",
            ),
            (
                Method::POST,
                "/api/v1/status",
                axum::http::StatusCode::METHOD_NOT_ALLOWED,
                "API method is not allowed",
            ),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .body(Body::empty())
                        .expect("routing contract request"),
                )
                .await
                .expect("routing contract response");
            assert_eq!(response.status(), status, "{uri}");
            assert_eq!(
                response.headers().get(header::CONTENT_TYPE),
                Some(&axum::http::HeaderValue::from_static("application/json")),
                "{uri}"
            );
            assert_eq!(
                serde_json::from_slice::<serde_json::Value>(
                    &to_bytes(response.into_body(), 1024)
                        .await
                        .expect("bounded routing body")
                )
                .expect("routing JSON"),
                serde_json::json!({ "error": error }),
                "{uri}"
            );
        }
    }

    #[tokio::test]
    async fn api_router_rejects_requests_after_lifecycle_drain_begins() {
        let fixture = TestFixture::new("shutdown-admission");
        let before = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .expect("status request"),
            )
            .await
            .expect("status response");
        assert_eq!(before.status(), axum::http::StatusCode::OK);
        drop(before);

        fixture.state.quiesce().await.expect("lifecycle quiesces");
        let after = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/status")
                    .body(Body::empty())
                    .expect("status request"),
            )
            .await
            .expect("shutdown response");
        assert_eq!(after.status(), axum::http::StatusCode::SERVICE_UNAVAILABLE);
        let body = to_bytes(after.into_body(), 1024)
            .await
            .expect("shutdown response body");
        let payload: serde_json::Value =
            serde_json::from_slice(&body).expect("shutdown response json");
        assert_eq!(
            payload,
            serde_json::json!({ "error": "application shutdown is in progress" })
        );
    }

    #[tokio::test]
    async fn admission_lease_is_held_until_inner_request_finishes() {
        let fixture = TestFixture::new("held-request-admission");
        let gate = Arc::new(RequestGate::default());
        let app = Router::new()
            .route("/api/v1/held", get(held_request))
            .layer(Extension(gate.clone()))
            .layer(middleware::from_fn_with_state(
                fixture.state.clone(),
                lifecycle_admission,
            ));
        let request = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .uri("/api/v1/held")
                    .body(Body::empty())
                    .expect("held request"),
            ),
        );
        gate.entered.notified().await;

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::DrainingRequests
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request drain begins");
        assert!(!quiesce.is_finished());

        gate.release.notify_one();
        assert_eq!(
            request
                .await
                .expect("held request task")
                .expect("held response")
                .status(),
            axum::http::StatusCode::NO_CONTENT
        );
        quiesce
            .await
            .expect("quiesce task")
            .expect("quiesce completes");
    }

    #[tokio::test]
    async fn admission_lease_is_held_until_streaming_response_finishes() {
        let fixture = TestFixture::new("streaming-response-admission");
        let gate = Arc::new(StreamGate::default());
        let app = Router::new()
            .route("/api/v1/stream", get(gated_stream))
            .layer(Extension(gate.clone()))
            .layer(middleware::from_fn_with_state(
                fixture.state.clone(),
                lifecycle_admission,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/stream")
                    .body(Body::empty())
                    .expect("stream request"),
            )
            .await
            .expect("stream response");
        let body = tokio::spawn(to_bytes(response.into_body(), 1024));
        gate.started.notified().await;

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::DrainingRequests
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stream request drain begins");
        assert!(!quiesce.is_finished());

        gate.release.notify_one();
        assert_eq!(
            body.await
                .expect("body task")
                .expect("stream body completes"),
            Bytes::from_static(b"complete")
        );
        quiesce
            .await
            .expect("quiesce task")
            .expect("quiesce follows stream completion");
    }

    #[tokio::test]
    async fn dropping_unpolled_streaming_response_releases_admission_lease() {
        let fixture = TestFixture::new("dropped-streaming-response-admission");
        let gate = Arc::new(StreamGate::default());
        let app = Router::new()
            .route("/api/v1/stream", get(gated_stream))
            .layer(Extension(gate))
            .layer(middleware::from_fn_with_state(
                fixture.state.clone(),
                lifecycle_admission,
            ));
        let response = app
            .oneshot(
                Request::builder()
                    .uri("/api/v1/stream")
                    .body(Body::empty())
                    .expect("stream request"),
            )
            .await
            .expect("stream response");

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::DrainingRequests
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("unpolled stream drain begins");
        assert!(!quiesce.is_finished());

        drop(response);
        quiesce
            .await
            .expect("quiesce task")
            .expect("dropping stream releases request");
    }

    #[tokio::test]
    async fn live_version_watch_finishes_when_request_drain_begins() {
        let fixture = TestFixture::new("version-watch-request-drain");
        fixture.state.configure_managed_library_for_test().await;

        let response = router(fixture.state.clone())
            .oneshot(
                Request::builder()
                    .uri("/api/v1/versions/watch")
                    .body(Body::empty())
                    .expect("version watch request"),
            )
            .await
            .expect("version watch response");
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let mut body = response.into_body();
        let initial_frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("initial version event arrives")
            .expect("version watch remains open")
            .expect("initial version frame");
        let initial_event = String::from_utf8(
            initial_frame
                .into_data()
                .expect("initial version event is data")
                .to_vec(),
        )
        .expect("initial version event is utf-8");
        assert!(initial_event.contains("event: versions_changed"));
        assert!(initial_event.contains("\"versions\":[]"));

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        let (remaining, quiesce_result) = tokio::time::timeout(Duration::from_secs(1), async {
            tokio::join!(to_bytes(body, 1024), quiesce)
        })
        .await
        .expect("version watch and quiescence complete");

        assert!(remaining.expect("remaining version watch body").is_empty());
        quiesce_result
            .expect("quiesce task")
            .expect("quiesce follows version watch completion");
        assert_eq!(
            fixture.state.lifecycle_phase(),
            crate::state::AppLifecyclePhase::Quiesced
        );
    }

    #[tokio::test]
    async fn live_request_handoff_is_the_only_producer_admission_during_drain() {
        let fixture = TestFixture::new("request-producer-handoff");
        let gate = Arc::new(HandoffGate::default());
        let app = Router::new()
            .route("/api/v1/handoff", get(request_handoff))
            .layer(Extension(gate.clone()))
            .layer(middleware::from_fn_with_state(
                fixture.state.clone(),
                lifecycle_admission,
            ));
        let request = tokio::spawn(
            app.oneshot(
                Request::builder()
                    .uri("/api/v1/handoff")
                    .body(Body::empty())
                    .expect("handoff request"),
            ),
        );
        gate.request_entered.notified().await;

        let shutdown_state = fixture.state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::DrainingRequests
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request drain begins");
        assert!(fixture.state.try_claim_producer().is_err());

        gate.claim.notify_one();
        gate.producer_started.notified().await;
        assert_eq!(
            request
                .await
                .expect("handoff request task")
                .expect("handoff response")
                .status(),
            axum::http::StatusCode::NO_CONTENT
        );
        tokio::time::timeout(Duration::from_secs(1), async {
            while fixture.state.lifecycle_phase()
                != crate::state::AppLifecyclePhase::QuiescingProducers
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("producer quiescence begins");
        assert!(!quiesce.is_finished());

        gate.producer_release.notify_one();
        quiesce
            .await
            .expect("quiesce task")
            .expect("quiesce completes");
    }

    #[derive(Default)]
    struct RequestGate {
        entered: Notify,
        release: Notify,
    }

    async fn held_request(Extension(gate): Extension<Arc<RequestGate>>) -> impl IntoResponse {
        gate.entered.notify_one();
        gate.release.notified().await;
        axum::http::StatusCode::NO_CONTENT
    }

    #[derive(Default)]
    struct StreamGate {
        started: Notify,
        release: Notify,
    }

    async fn gated_stream(Extension(gate): Extension<Arc<StreamGate>>) -> impl IntoResponse {
        let stream = async_stream::stream! {
            gate.started.notify_one();
            gate.release.notified().await;
            yield Ok::<Bytes, Infallible>(Bytes::from_static(b"complete"));
        };
        Body::from_stream(stream)
    }

    #[derive(Default)]
    struct HandoffGate {
        request_entered: Notify,
        claim: Notify,
        producer_started: Notify,
        producer_release: Notify,
    }

    async fn request_handoff(
        Extension(gate): Extension<Arc<HandoffGate>>,
        Extension(handoff): Extension<RequestProducerHandoff>,
    ) -> impl IntoResponse {
        gate.request_entered.notify_one();
        gate.claim.notified().await;
        let producer = handoff
            .try_claim()
            .expect("live request handoff remains authorized while draining");
        let producer_gate = gate.clone();
        producer.spawn(async move {
            producer_gate.producer_started.notify_one();
            producer_gate.producer_release.notified().await;
        });
        axum::http::StatusCode::NO_CONTENT
    }

    struct TestFixture {
        state: AppState,
        root: PathBuf,
    }

    impl TestFixture {
        fn new(name: &str) -> Self {
            let root = test_root(name);
            let paths = test_paths(&root);
            let root_session = crate::state::test_root_session(&paths);
            let config = Arc::new(
                ConfigStore::load_from(paths.clone(), Arc::clone(&root_session))
                    .expect("load config"),
            );
            let instances = Arc::new(
                InstanceStore::from_snapshot(
                    paths.clone(),
                    root_session,
                    InstanceRegistrySnapshot::default(),
                )
                .expect("load instances"),
            );
            let state = AppState::new(AppStateInit {
                app_name: "Axial".to_string(),
                version: "test".to_string(),
                config,
                instances,
                installs: Arc::new(InstallStore::new()),
                sessions: Arc::new(SessionStore::new()),
                performance: Arc::new(
                    PerformanceManager::load_for_startup(paths.performance_dir())
                        .expect("performance manager"),
                ),
                startup_warnings: Vec::new(),
            });
            Self { state, root }
        }
    }

    impl Drop for TestFixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.root);
        }
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }

    fn test_root(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "axial-api-lifecycle-{name}-{}-{nonce}",
            std::process::id()
        ))
    }
}
