mod accounts;
mod auth;
mod catalog;
mod config;
mod content;
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

use crate::state::{AppState, LifecycleAdmissionError, RequestLease};
use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, header},
    middleware::{self, Next},
    response::{IntoResponse, Response},
};
use http_body_util::BodyExt;
use tower_http::cors::{AllowOrigin, CorsLayer};

pub fn router(state: AppState) -> Router {
    let admission_state = state.clone();
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
        .with_state(state)
        .layer(middleware::from_fn_with_state(
            admission_state,
            lifecycle_admission,
        ))
        .layer(local_cors_layer())
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

fn local_cors_layer() -> CorsLayer {
    CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|origin, _| {
            is_allowed_local_origin(origin)
        }))
        .allow_methods([
            Method::GET,
            Method::POST,
            Method::PUT,
            Method::PATCH,
            Method::DELETE,
            Method::OPTIONS,
        ])
        .allow_headers([header::CONTENT_TYPE])
}

fn is_allowed_local_origin(origin: &HeaderValue) -> bool {
    let Ok(origin) = origin.to_str() else {
        return false;
    };

    origin == "tauri://localhost"
        || origin == "http://tauri.localhost"
        || origin == "https://tauri.localhost"
        || origin
            .strip_prefix("http://127.0.0.1:")
            .is_some_and(is_port_suffix)
        || origin
            .strip_prefix("http://localhost:")
            .is_some_and(is_port_suffix)
        || origin
            .strip_prefix("http://[::1]:")
            .is_some_and(is_port_suffix)
}

fn is_port_suffix(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit())
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
        let library_dir = fixture.root.join("library");
        fs::create_dir_all(&library_dir).expect("create version library");
        fixture
            .state
            .set_library_dir_for_test(library_dir.to_string_lossy().into_owned());

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
            let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
            let instances = Arc::new(
                InstanceStore::from_snapshot(paths.clone(), InstanceRegistrySnapshot::default())
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
                    PerformanceManager::load_for_startup(&paths.config_dir)
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
        let config_dir = root.join("config");
        AppPaths {
            config_file: config_dir.join("config.json"),
            instances_file: config_dir.join("instances.json"),
            instances_dir: root.join("instances"),
            music_dir: root.join("music"),
            library_dir: root.join("library"),
            config_dir,
        }
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
