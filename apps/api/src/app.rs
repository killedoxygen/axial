use crate::observability::telemetry::{
    TelemetryEvent, install_panic_capture, run_panic_capture_loop, run_telemetry_flush_loop,
};
use crate::routes;
use crate::state::AppState;
use crate::transport::{ApiTransportBootstrap, LocalApiAuthority};
use axum::Router;
#[cfg(feature = "embedded-frontend")]
use axum::{
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
};
#[cfg(feature = "embedded-frontend")]
use include_dir::{Dir, include_dir};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::sync::watch;

pub const DEFAULT_API_PORT: u16 = 43_430;
pub const PERFORMANCE_RULES_REFRESH_INTERVAL_ENV: &str =
    "AXIAL_PERFORMANCE_RULES_REFRESH_INTERVAL_SECONDS";
pub const MIN_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
pub const MAX_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
const EMBEDDED_API_SHUTDOWN_GRACE: Duration = Duration::from_secs(5);
#[cfg(feature = "embedded-frontend")]
static EMBEDDED_FRONTEND: Dir<'_> = include_dir!("$OUT_DIR/embedded-frontend");

pub fn build_router(state: AppState, authority: LocalApiAuthority) -> Router {
    let api = routes::router_with_authority(state, authority);
    #[cfg(feature = "embedded-frontend")]
    {
        api.fallback(get(serve_embedded_frontend))
    }
    #[cfg(not(feature = "embedded-frontend"))]
    {
        api
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OptionalWorkflowState {
    Disabled,
    Started,
    AdmissionFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ApplicationStartupHealth {
    pub known_good_rebuilds: OptionalWorkflowState,
    pub idle_integrity: OptionalWorkflowState,
    pub performance_operations: OptionalWorkflowState,
    pub benchmark_suite_drivers: OptionalWorkflowState,
    pub performance_rules: OptionalWorkflowState,
    pub telemetry: OptionalWorkflowState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Error)]
pub enum ApplicationStartupError {
    #[error("Guardian failure-memory startup settlement is incomplete")]
    GuardianFailureMemory,
    #[error("install startup recovery is incomplete")]
    InstallRecovery,
    #[error("version-bundle startup settlement is incomplete")]
    VersionBundlePublications,
    #[error("persisted-state startup repair is incomplete")]
    PersistedStateRepairs,
}

pub async fn start_application_background_workflows(
    state: &AppState,
) -> Result<ApplicationStartupHealth, ApplicationStartupError> {
    settle_startup_publication_barriers(
        state,
        crate::application::rehydrate_startup_installs(state),
    )
    .await?;
    if !crate::application::settle_startup_persisted_state_repairs(state).await {
        return Err(ApplicationStartupError::PersistedStateRepairs);
    }
    crate::application::cleanup_update_staging(state).await;
    Ok(ApplicationStartupHealth {
        known_good_rebuilds: spawn_known_good_rebuilds(state),
        idle_integrity: spawn_idle_integrity_scheduler(state),
        performance_operations: spawn_performance_operations_resume(state),
        benchmark_suite_drivers: spawn_benchmark_suite_drivers_resume(state),
        performance_rules: spawn_performance_rules_refresh(state),
        telemetry: spawn_telemetry_export(state),
    })
}

pub(crate) async fn settle_startup_publication_barriers<InstallRecovery>(
    state: &AppState,
    install_recovery: InstallRecovery,
) -> Result<(), ApplicationStartupError>
where
    InstallRecovery: Future<Output = bool>,
{
    if !crate::application::settle_startup_install_guardian_failure_memory(state).await {
        return Err(ApplicationStartupError::GuardianFailureMemory);
    }
    if !install_recovery.await {
        return Err(ApplicationStartupError::InstallRecovery);
    }
    if !crate::application::settle_startup_version_bundle_publications(state).await {
        return Err(ApplicationStartupError::VersionBundlePublications);
    }
    Ok(())
}

fn spawn_performance_rules_refresh(state: &AppState) -> OptionalWorkflowState {
    if !state.performance().remote_refresh_enabled() {
        return OptionalWorkflowState::Disabled;
    }
    let Ok(producer) = state.try_claim_producer() else {
        return OptionalWorkflowState::AdmissionFailed;
    };
    let shutdown = state.subscribe_shutdown();

    let interval = configured_performance_rules_refresh_interval();
    tracing::info!(
        interval_seconds = interval.as_secs(),
        "performance rules periodic refresh scheduled"
    );
    let state = state.clone();
    producer.spawn(run_periodic_refresh_loop(
        move || {
            let state = state.clone();
            async move {
                refresh_performance_rules_once(&state).await;
            }
        },
        interval,
        shutdown,
    ));

    OptionalWorkflowState::Started
}

fn spawn_telemetry_export(state: &AppState) -> OptionalWorkflowState {
    install_panic_capture(state.telemetry().clone());
    state.telemetry().emit(TelemetryEvent::app_started(
        state.version(),
        &state.config().current(),
    ));
    if !state.telemetry().export_configured() {
        return OptionalWorkflowState::Disabled;
    }
    let Ok(producer) = state.try_claim_producer() else {
        return OptionalWorkflowState::AdmissionFailed;
    };

    let telemetry = state.telemetry().clone();
    let shutdown = state.subscribe_shutdown();
    if let Some(receiver) = telemetry.take_panic_receiver() {
        producer.spawn_child(run_panic_capture_loop(
            telemetry.clone(),
            receiver,
            shutdown.clone(),
        ));
    }
    producer.spawn(run_telemetry_flush_loop(telemetry, shutdown));
    OptionalWorkflowState::Started
}

async fn run_periodic_refresh_loop<F, Fut>(
    mut refresh: F,
    interval: Duration,
    mut shutdown: watch::Receiver<bool>,
) where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        if *shutdown.borrow_and_update() {
            return;
        }
        refresh().await;
        if wait_for_shutdown(&mut shutdown, interval).await {
            return;
        }
    }
}

async fn wait_for_shutdown(shutdown: &mut watch::Receiver<bool>, delay: Duration) -> bool {
    if *shutdown.borrow_and_update() {
        return true;
    }
    tokio::select! {
        _ = tokio::time::sleep(delay) => *shutdown.borrow_and_update(),
        changed = shutdown.changed() => {
            changed.is_err() || *shutdown.borrow_and_update()
        }
    }
}

async fn refresh_performance_rules_once(state: &AppState) {
    match state.refresh_performance_rules().await {
        Ok(status) => {
            if status.warnings.is_empty() {
                tracing::info!(
                    rule_source = ?status.rule_source,
                    generated_at = %status.generated_at,
                    "performance rules background refresh finished"
                );
            } else {
                tracing::warn!(
                    warnings = ?status.warnings,
                    "performance rules background refresh finished with warnings"
                );
            }
        }
        Err(error) => {
            let warning = performance_rules_refresh_log_warning(&error);
            tracing::warn!(%warning, "performance rules background refresh failed");
        }
    }
}

fn performance_rules_refresh_log_warning(error: &axial_performance::RulesRefreshError) -> String {
    axial_performance::remote_rules_refresh_warning("failed", error)
}

fn configured_performance_rules_refresh_interval() -> Duration {
    parse_performance_rules_refresh_interval(
        std::env::var(PERFORMANCE_RULES_REFRESH_INTERVAL_ENV)
            .ok()
            .as_deref(),
    )
}

fn parse_performance_rules_refresh_interval(value: Option<&str>) -> Duration {
    let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
        return DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL;
    };
    let Ok(seconds) = value.parse::<u64>() else {
        return DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL;
    };
    Duration::from_secs(seconds).clamp(
        MIN_PERFORMANCE_RULES_REFRESH_INTERVAL,
        MAX_PERFORMANCE_RULES_REFRESH_INTERVAL,
    )
}

fn spawn_performance_operations_resume(state: &AppState) -> OptionalWorkflowState {
    let Ok(producer) = state.try_claim_producer() else {
        return OptionalWorkflowState::AdmissionFailed;
    };
    crate::application::spawn_pending_performance_operations(state, producer);
    OptionalWorkflowState::Started
}

fn spawn_known_good_rebuilds(state: &AppState) -> OptionalWorkflowState {
    let Ok(producer) = state.try_claim_producer() else {
        return OptionalWorkflowState::AdmissionFailed;
    };
    if crate::application::spawn_startup_known_good_rebuilds(state, producer) {
        OptionalWorkflowState::Started
    } else {
        OptionalWorkflowState::Disabled
    }
}

fn spawn_idle_integrity_scheduler(state: &AppState) -> OptionalWorkflowState {
    let Ok(producer) = state.try_claim_producer() else {
        return OptionalWorkflowState::AdmissionFailed;
    };
    crate::application::spawn_idle_integrity_scheduler(state, producer);
    OptionalWorkflowState::Started
}

fn spawn_benchmark_suite_drivers_resume(state: &AppState) -> OptionalWorkflowState {
    if crate::application::launch::spawn_restart_interrupted_benchmark_suite_drivers(state) {
        OptionalWorkflowState::Started
    } else {
        OptionalWorkflowState::AdmissionFailed
    }
}

#[derive(Debug)]
pub struct ServerHandle {
    pub addr: SocketAddr,
    authority: LocalApiAuthority,
    shutdown: watch::Sender<bool>,
    completion: watch::Receiver<ServerCompletion>,
}

#[derive(Debug, Error)]
pub enum ApiServerError {
    #[error("invalid local API authority: {0}")]
    Authority(String),
    #[error("failed to bind listener: {0}")]
    Bind(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Error)]
pub enum ApiServerShutdownError {
    #[error("embedded API server stopped with an error")]
    Serve,
    #[error("embedded API server task stopped unexpectedly")]
    Task,
    #[error("embedded API server exceeded its graceful shutdown deadline")]
    Forced,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ServerCompletion {
    Running,
    Stopped,
    ServeFailed,
    TaskStopped,
    Forced,
}

impl ServerHandle {
    pub fn transport_bootstrap(&self) -> ApiTransportBootstrap {
        self.authority.bootstrap()
    }

    pub async fn wait(&self) -> Result<(), ApiServerShutdownError> {
        let mut completion = self.completion.clone();
        loop {
            match *completion.borrow_and_update() {
                ServerCompletion::Running => {}
                ServerCompletion::Stopped => return Ok(()),
                ServerCompletion::ServeFailed => return Err(ApiServerShutdownError::Serve),
                ServerCompletion::TaskStopped => return Err(ApiServerShutdownError::Task),
                ServerCompletion::Forced => return Err(ApiServerShutdownError::Forced),
            }
            completion
                .changed()
                .await
                .map_err(|_| ApiServerShutdownError::Task)?;
        }
    }

    pub async fn shutdown(&self) -> Result<(), ApiServerShutdownError> {
        // The detached supervisor owns the server task. Once signalled, caller
        // cancellation cannot abandon either graceful shutdown or the join.
        let _ = self.shutdown.send(true);
        self.wait().await
    }
}

impl Drop for ServerHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
    }
}

pub async fn spawn_background(state: AppState) -> Result<ServerHandle, ApiServerError> {
    spawn_background_for_origin(state, None).await
}

pub async fn spawn_background_for_origin(
    state: AppState,
    origin: Option<&str>,
) -> Result<ServerHandle, ApiServerError> {
    spawn_background_on_with_origin(state, SocketAddr::from(([127, 0, 0, 1], 0)), origin).await
}

pub async fn spawn_background_on(
    state: AppState,
    addr: SocketAddr,
) -> Result<ServerHandle, ApiServerError> {
    spawn_background_on_with_origin(state, addr, None).await
}

async fn spawn_background_on_with_origin(
    state: AppState,
    addr: SocketAddr,
    origin: Option<&str>,
) -> Result<ServerHandle, ApiServerError> {
    require_loopback(addr)?;
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let authority = LocalApiAuthority::new(addr, origin).map_err(ApiServerError::Authority)?;
    let router = build_router(state, authority.clone());
    spawn_bound_router(
        router,
        listener,
        addr,
        authority,
        EMBEDDED_API_SHUTDOWN_GRACE,
    )
    .await
}

#[cfg(test)]
async fn spawn_background_router(
    router: Router,
    addr: SocketAddr,
) -> Result<ServerHandle, ApiServerError> {
    spawn_background_router_with_grace(router, addr, EMBEDDED_API_SHUTDOWN_GRACE).await
}

#[cfg(test)]
async fn spawn_background_router_with_grace(
    router: Router,
    addr: SocketAddr,
    shutdown_grace: Duration,
) -> Result<ServerHandle, ApiServerError> {
    require_loopback(addr)?;
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let authority = LocalApiAuthority::new(addr, None).map_err(ApiServerError::Authority)?;
    spawn_bound_router(router, listener, addr, authority, shutdown_grace).await
}

async fn spawn_bound_router(
    router: Router,
    listener: TcpListener,
    addr: SocketAddr,
    authority: LocalApiAuthority,
    shutdown_grace: Duration,
) -> Result<ServerHandle, ApiServerError> {
    let (shutdown, shutdown_rx) = watch::channel(false);
    let (completion_tx, completion) = watch::channel(ServerCompletion::Running);
    let mut server_shutdown_rx = shutdown_rx.clone();
    let mut task = tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(async move {
                while !*server_shutdown_rx.borrow_and_update() {
                    if server_shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });
    tokio::spawn(async move {
        let mut shutdown_rx = shutdown_rx;
        let (result, forced) = tokio::select! {
            result = &mut task => (result, false),
            _ = async {
                while !*shutdown_rx.borrow_and_update() {
                    if shutdown_rx.changed().await.is_err() {
                        break;
                    }
                }
            } => {
                match tokio::time::timeout(shutdown_grace, &mut task).await {
                    Ok(result) => (result, false),
                    Err(_) => {
                        task.abort();
                        (task.await, true)
                    }
                }
            }
        };
        let completion = match (forced, result) {
            (true, _) => ServerCompletion::Forced,
            (false, Ok(Ok(()))) => ServerCompletion::Stopped,
            (false, Ok(Err(error))) => {
                tracing::error!(error_kind = ?error.kind(), "embedded API server stopped");
                ServerCompletion::ServeFailed
            }
            (false, Err(error)) if error.is_cancelled() && *shutdown_rx.borrow() => {
                ServerCompletion::Stopped
            }
            (false, Err(_)) => ServerCompletion::TaskStopped,
        };
        let _ = completion_tx.send(completion);
    });

    Ok(ServerHandle {
        addr,
        authority,
        shutdown,
        completion,
    })
}

fn require_loopback(addr: SocketAddr) -> Result<(), ApiServerError> {
    if addr.ip().is_loopback() {
        Ok(())
    } else {
        Err(ApiServerError::Authority(
            "local API address must be loopback".to_string(),
        ))
    }
}

#[cfg(feature = "embedded-frontend")]
async fn serve_embedded_frontend(uri: Uri) -> impl IntoResponse {
    let path = normalized_embedded_path(uri.path());
    let file = EMBEDDED_FRONTEND
        .get_file(&path)
        .or_else(|| EMBEDDED_FRONTEND.get_file("index.html"));

    match file {
        Some(file) => Response::builder()
            .status(StatusCode::OK)
            .header(
                header::CONTENT_TYPE,
                content_type_for_path(file.path().to_string_lossy().as_ref()),
            )
            .body(Body::from(file.contents()))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(feature = "embedded-frontend")]
fn normalized_embedded_path(path: &str) -> String {
    let trimmed = path.trim_start_matches('/');
    if trimmed.is_empty() {
        return "index.html".to_string();
    }
    if EMBEDDED_FRONTEND.get_file(trimmed).is_some() {
        return trimmed.to_string();
    }
    "index.html".to_string()
}

#[cfg(feature = "embedded-frontend")]
fn content_type_for_path(path: &str) -> String {
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::axial_api_test_support::build_test_state;
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use axum::routing::get;
    #[cfg(feature = "embedded-frontend")]
    use http_body_util::BodyExt;
    use std::fs;
    #[cfg(feature = "embedded-frontend")]
    use std::path::Path;
    use std::sync::Arc;
    use tokio::io::AsyncWriteExt;
    use tokio::sync::{Notify, mpsc};
    use tower::ServiceExt;

    #[cfg(feature = "embedded-frontend")]
    fn collect_embedded_files<'a>(
        directory: &'a Dir<'a>,
        files: &mut Vec<&'a include_dir::File<'a>>,
    ) {
        files.extend(directory.files());
        for child in directory.dirs() {
            collect_embedded_files(child, files);
        }
    }

    #[cfg(feature = "embedded-frontend")]
    #[test]
    fn embedded_frontend_is_byte_exact_and_manifest_reachable() {
        let manifest_file = EMBEDDED_FRONTEND
            .get_file("generation.json")
            .expect("embedded generation manifest");
        let manifest: serde_json::Value =
            serde_json::from_slice(manifest_file.contents()).expect("parse generation manifest");
        let records = manifest["files"].as_array().expect("generation files");
        let source_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../frontend/dist");
        let mut expected = vec!["generation.json".to_string()];
        for record in records {
            let relative = record["path"].as_str().expect("generation file path");
            let embedded = EMBEDDED_FRONTEND
                .get_file(relative)
                .expect("manifest-reachable embedded file");
            let filesystem =
                fs::read(source_root.join(relative)).expect("filesystem generation file");
            assert_eq!(
                embedded.contents(),
                filesystem,
                "embedded byte drift for {relative}"
            );
            expected.push(relative.to_string());
        }
        let mut embedded = Vec::new();
        collect_embedded_files(&EMBEDDED_FRONTEND, &mut embedded);
        let mut actual = embedded
            .into_iter()
            .map(|file| file.path().to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
        assert_eq!(
            manifest_file.contents(),
            fs::read(source_root.join("generation.json")).unwrap()
        );
    }

    #[cfg(feature = "embedded-frontend")]
    #[tokio::test]
    async fn p02_b06_contract_cross_owner_embedded_static_routing_and_mime_are_exact() {
        for (request_path, expected_type) in [
            ("/index.html", "text/html"),
            ("/app.js", "text/javascript"),
            ("/app.css", "text/css"),
            ("/fonts/GeistMono-Variable.woff2", "font/woff2"),
            ("/sounds/snd01/audioSprite.mp3", "audio/mpeg"),
            ("/sounds/snd01/audioSprite.json", "application/json"),
            ("/generation.json", "application/json"),
        ] {
            let response = serve_embedded_frontend(request_path.parse::<Uri>().unwrap())
                .await
                .into_response();
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(response.headers()[header::CONTENT_TYPE], expected_type);
            let expected = EMBEDDED_FRONTEND
                .get_file(request_path.trim_start_matches('/'))
                .unwrap()
                .contents();
            assert_eq!(
                response.into_body().collect().await.unwrap().to_bytes(),
                expected
            );
        }

        let response = serve_embedded_frontend(Uri::from_static("/missing/route"))
            .await
            .into_response();
        assert_eq!(response.headers()[header::CONTENT_TYPE], "text/html");
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            EMBEDDED_FRONTEND.get_file("index.html").unwrap().contents()
        );

        let root = super::axial_api_test_support::test_root("routing-static-contract");
        let app = build_router(
            build_test_state(&root, None),
            LocalApiAuthority::bypass_for_test(),
        );
        let api_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/api/v2/not-a-real-route")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(api_response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            api_response.headers()[header::CONTENT_TYPE],
            "application/json"
        );
        assert_eq!(
            api_response.into_body().collect().await.unwrap().to_bytes(),
            r#"{"error":"API route was not found"}"#
        );

        let head_response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::HEAD)
                    .uri("/instances/example/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(head_response.status(), StatusCode::OK);
        assert_eq!(head_response.headers()[header::CONTENT_TYPE], "text/html");
        assert!(
            head_response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes()
                .is_empty()
        );

        let post_response = app
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::POST)
                    .uri("/instances/example/settings")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(post_response.status(), StatusCode::METHOD_NOT_ALLOWED);
        assert_ne!(
            post_response
                .into_body()
                .collect()
                .await
                .unwrap()
                .to_bytes(),
            EMBEDDED_FRONTEND.get_file("index.html").unwrap().contents()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(feature = "embedded-frontend")]
    #[tokio::test]
    async fn standalone_router_owns_only_the_embedded_frontend_fallback() {
        let root = super::axial_api_test_support::test_root("embedded-router");
        let response = build_router(
            build_test_state(&root, None),
            LocalApiAuthority::bypass_for_test(),
        )
        .oneshot(
            Request::builder()
                .uri("/not-an-api-route")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.into_body().collect().await.unwrap().to_bytes(),
            EMBEDDED_FRONTEND.get_file("index.html").unwrap().contents()
        );
        let _ = fs::remove_dir_all(root);
    }

    #[cfg(not(feature = "embedded-frontend"))]
    #[tokio::test]
    async fn desktop_api_router_has_no_frontend_fallback() {
        let root = super::axial_api_test_support::test_root("no-frontend-router");
        let response = build_router(
            build_test_state(&root, None),
            LocalApiAuthority::bypass_for_test(),
        )
        .oneshot(
            Request::builder()
                .uri("/index.html")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn performance_rules_refresh_interval_defaults_and_clamps() {
        assert_eq!(
            parse_performance_rules_refresh_interval(None),
            DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL
        );
        assert_eq!(
            parse_performance_rules_refresh_interval(Some(" \t\n ")),
            DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL
        );
        assert_eq!(
            parse_performance_rules_refresh_interval(Some("not-seconds")),
            DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL
        );
        assert_eq!(
            parse_performance_rules_refresh_interval(Some("1")),
            MIN_PERFORMANCE_RULES_REFRESH_INTERVAL
        );
        assert_eq!(
            parse_performance_rules_refresh_interval(Some("1800")),
            Duration::from_secs(30 * 60)
        );
        assert_eq!(
            parse_performance_rules_refresh_interval(Some("999999999")),
            MAX_PERFORMANCE_RULES_REFRESH_INTERVAL
        );
    }

    #[test]
    fn performance_rules_refresh_log_warning_omits_raw_persistence_details() {
        let error = axial_performance::RulesRefreshError::Cache(std::io::Error::other(
            "failed /private/rules-cache.json?token=secret-token",
        ));

        let warning = performance_rules_refresh_log_warning(&error);

        assert!(warning.contains("remote rules cache could not be persisted"));
        assert!(!warning.contains("private"));
        assert!(!warning.contains("rules-cache.json"));
        assert!(!warning.contains("secret-token"));
    }

    #[tokio::test]
    async fn performance_rules_refresh_spawns_only_when_remote_url_is_configured() {
        let unset_root = axial_api_test_support::test_root("app-refresh-unset");
        let unset_state = build_test_state(&unset_root, None);
        assert_eq!(
            spawn_performance_rules_refresh(&unset_state),
            OptionalWorkflowState::Disabled
        );
        unset_state
            .shutdown()
            .await
            .expect("unset state shuts down");
        drop(unset_state);
        let _ = fs::remove_dir_all(&unset_root);

        let configured_root = axial_api_test_support::test_root("app-refresh-configured");
        let configured_state = build_test_state(
            &configured_root,
            Some("http://127.0.0.1:9/rules.json".to_string()),
        );
        assert_eq!(
            spawn_performance_rules_refresh(&configured_state),
            OptionalWorkflowState::Started
        );
        configured_state
            .quiesce()
            .await
            .expect("configured state quiesces");
        configured_state
            .shutdown()
            .await
            .expect("configured state shuts down");
        drop(configured_state);
        let _ = fs::remove_dir_all(&configured_root);
    }

    #[tokio::test]
    async fn p02_b07_contract_optional_workflow_reports_admission_failure_during_request_drain() {
        let root = axial_api_test_support::test_root("performance-resume-draining");
        let state = build_test_state(&root, None);
        let request = state.try_admit_request().expect("admit held request");
        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.lifecycle_phase() != crate::state::AppLifecyclePhase::DrainingRequests {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request drain begins");

        assert_eq!(
            spawn_performance_operations_resume(&state),
            OptionalWorkflowState::AdmissionFailed
        );
        drop(request);
        quiesce
            .await
            .expect("quiesce task")
            .expect("quiesce completes");
        state.shutdown().await.expect("state shuts down");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test(start_paused = true)]
    async fn performance_rules_refresh_loop_runs_initially_and_after_interval() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let (shutdown_tx, shutdown_rx) = watch::channel(false);
        let task = tokio::spawn(run_periodic_refresh_loop(
            move || {
                let tx = tx.clone();
                async move {
                    tx.send(()).expect("record refresh tick");
                }
            },
            Duration::from_secs(60),
            shutdown_rx,
        ));

        rx.recv().await.expect("initial refresh tick");
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());

        tokio::time::advance(Duration::from_secs(1)).await;
        rx.recv().await.expect("periodic refresh tick");
        shutdown_tx.send_replace(true);
        task.await.expect("periodic refresh loop stops");
    }

    #[tokio::test]
    async fn quiesce_waits_for_inflight_periodic_refresh_then_stops_the_loop() {
        let root = axial_api_test_support::test_root("app-refresh-quiesce");
        let state = build_test_state(&root, None);
        let producer = state.try_claim_producer().expect("claim periodic producer");
        let shutdown = state.subscribe_shutdown();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut entered_tx = Some(entered_tx);
        let mut release_rx = Some(release_rx);
        producer.spawn(run_periodic_refresh_loop(
            move || {
                let entered_tx = entered_tx.take().expect("single refresh entry");
                let release_rx = release_rx.take().expect("single refresh release");
                async move {
                    let _ = entered_tx.send(());
                    let _ = release_rx.await;
                }
            },
            Duration::from_secs(60),
            shutdown,
        ));
        entered_rx.await.expect("periodic refresh entered");

        let shutdown_state = state.clone();
        let quiesce = tokio::spawn(async move { shutdown_state.quiesce().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            while state.lifecycle_phase() != crate::state::AppLifecyclePhase::QuiescingProducers {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("producer quiescence begins");
        assert!(!quiesce.is_finished());

        release_tx.send(()).expect("release periodic refresh");
        tokio::time::timeout(Duration::from_secs(1), quiesce)
            .await
            .expect("quiesce completion deadline")
            .expect("quiesce task")
            .expect("quiesce completes");
        state.shutdown().await.expect("state shuts down");
        drop(state);
        let _ = fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn embedded_server_shutdown_is_cancellation_owned_concurrent_and_idempotent() {
        let entered = Arc::new(Notify::new());
        let release = Arc::new(Notify::new());
        let route_entered = entered.clone();
        let route_release = release.clone();
        let router = Router::new().route(
            "/hold",
            get(move || {
                let entered = route_entered.clone();
                let release = route_release.clone();
                async move {
                    entered.notify_one();
                    release.notified().await;
                    "done"
                }
            }),
        );
        let server = Arc::new(
            spawn_background_router(router, SocketAddr::from(([127, 0, 0, 1], 0)))
                .await
                .expect("spawn embedded API"),
        );
        let mut connection = tokio::net::TcpStream::connect(server.addr)
            .await
            .expect("connect embedded API");
        connection
            .write_all(b"GET /hold HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("start an in-flight request");
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("request entered handler");

        let first_server = server.clone();
        let first = tokio::spawn(async move { first_server.shutdown().await });
        tokio::time::timeout(Duration::from_secs(1), async {
            let mut shutdown = server.shutdown.subscribe();
            while !*shutdown.borrow_and_update() {
                shutdown.changed().await.expect("shutdown signal");
            }
        })
        .await
        .expect("shutdown is signalled before waiting");
        first.abort();
        assert!(
            first
                .await
                .expect_err("shutdown waiter cancelled")
                .is_cancelled()
        );
        release.notify_one();
        drop(connection);

        let (second, third) = tokio::join!(server.shutdown(), server.shutdown());
        assert_eq!(second, Ok(()));
        assert_eq!(third, Ok(()));
        assert_eq!(server.shutdown().await, Ok(()));
        assert!(tokio::net::TcpStream::connect(server.addr).await.is_err());
    }

    #[tokio::test]
    async fn embedded_server_reports_forced_stop_after_grace_deadline() {
        let entered = Arc::new(Notify::new());
        let route_entered = entered.clone();
        let router = Router::new().route(
            "/hold",
            get(move || {
                let entered = route_entered.clone();
                async move {
                    entered.notify_one();
                    std::future::pending::<&'static str>().await
                }
            }),
        );
        let server = spawn_background_router_with_grace(
            router,
            SocketAddr::from(([127, 0, 0, 1], 0)),
            Duration::from_millis(20),
        )
        .await
        .expect("spawn embedded API");
        let mut connection = tokio::net::TcpStream::connect(server.addr)
            .await
            .expect("connect embedded API");
        connection
            .write_all(b"GET /hold HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await
            .expect("start held request");
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .expect("request entered handler");

        assert_eq!(
            tokio::time::timeout(Duration::from_secs(1), server.shutdown())
                .await
                .expect("forced shutdown deadline"),
            Err(ApiServerShutdownError::Forced)
        );
        assert_eq!(server.wait().await, Err(ApiServerShutdownError::Forced));
        assert!(tokio::net::TcpStream::connect(server.addr).await.is_err());
        drop(connection);
    }
}

#[cfg(test)]
mod axial_api_test_support {
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore, test_root_session};
    use axial_config::{AppPaths, ConfigStore, InstanceRegistrySnapshot, InstanceStore};
    use axial_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    pub fn build_test_state(root: &Path, remote_rules_url: Option<String>) -> AppState {
        let paths = test_paths(root);
        let root_session = test_root_session(&paths);
        let config = Arc::new(
            ConfigStore::load_from(paths.clone(), Arc::clone(&root_session)).expect("load config"),
        );
        let instances = Arc::new(
            InstanceStore::from_snapshot(
                paths.clone(),
                root_session,
                InstanceRegistrySnapshot::default(),
            )
            .expect("load instances"),
        );
        AppState::new(AppStateInit {
            app_name: "Axial".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::load_for_startup_with_remote_url(
                    paths.performance_dir(),
                    remote_rules_url,
                )
                .expect("performance manager"),
            ),
            startup_warnings: Vec::new(),
        })
    }

    pub fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "axial-api-app-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|value| value.as_nanos())
                .unwrap_or_default()
        ));
        fs::create_dir_all(&path).expect("create test root");
        path
    }

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths::from_root(root.to_path_buf()).expect("absolute test app root")
    }
}
