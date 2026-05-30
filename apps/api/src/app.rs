use crate::routes;
use crate::state::AppState;
use axum::{
    Router,
    body::Body,
    http::{Response, StatusCode, Uri, header},
    response::IntoResponse,
    routing::get,
};
use include_dir::{Dir, include_dir};
use std::future::Future;
use std::io;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use thiserror::Error;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tower_http::services::{ServeDir, ServeFile};

pub const DEFAULT_API_PORT: u16 = 43_430;
pub const PERFORMANCE_RULES_REFRESH_INTERVAL_ENV: &str =
    "CROOPOR_PERFORMANCE_RULES_REFRESH_INTERVAL_SECONDS";
pub const MIN_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(15 * 60);
pub const DEFAULT_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(6 * 60 * 60);
pub const MAX_PERFORMANCE_RULES_REFRESH_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
static EMBEDDED_FRONTEND: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../../frontend/static");

pub fn default_frontend_dir() -> PathBuf {
    std::env::var("CROOPOR_FRONTEND_STATIC_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("frontend/static"))
}

pub fn build_router(state: AppState) -> Router {
    let frontend_dir = state.frontend_dir().to_path_buf();
    let index_path = frontend_dir.join("index.html");
    let api = routes::router(state);

    if frontend_dir.is_dir() && index_path.is_file() {
        let static_service =
            ServeDir::new(frontend_dir).not_found_service(ServeFile::new(index_path));
        api.fallback_service(static_service)
    } else {
        api.fallback(get(serve_embedded_frontend))
    }
}

pub fn spawn_performance_rules_refresh(state: &AppState) -> bool {
    let performance = state.performance().clone();
    if !performance.remote_refresh_enabled() {
        return false;
    }

    let interval = configured_performance_rules_refresh_interval();
    tracing::info!(
        interval_seconds = interval.as_secs(),
        "performance rules periodic refresh scheduled"
    );
    tokio::spawn(run_performance_rules_refresh_loop(
        move || {
            let performance = performance.clone();
            async move {
                refresh_performance_rules_once(&performance).await;
            }
        },
        interval,
    ));

    true
}

async fn run_performance_rules_refresh_loop<F, Fut>(mut refresh: F, interval: Duration)
where
    F: FnMut() -> Fut + Send + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    loop {
        refresh().await;
        tokio::time::sleep(interval).await;
    }
}

async fn refresh_performance_rules_once(performance: &croopor_performance::PerformanceManager) {
    match performance.refresh_rules().await {
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
            tracing::warn!("performance rules background refresh failed: {error}");
        }
    }
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

pub fn spawn_performance_operations_resume(state: &AppState) -> bool {
    crate::routes::spawn_pending_performance_operations(state)
}

#[derive(Debug)]
pub struct ServerHandle {
    pub addr: SocketAddr,
    pub task: JoinHandle<()>,
}

#[derive(Debug, Error)]
pub enum ApiServerError {
    #[error("failed to bind listener: {0}")]
    Bind(#[from] io::Error),
}

pub async fn spawn_background(state: AppState) -> Result<ServerHandle, ApiServerError> {
    spawn_background_on(state, SocketAddr::from(([127, 0, 0, 1], 0))).await
}

pub async fn spawn_background_on(
    state: AppState,
    addr: SocketAddr,
) -> Result<ServerHandle, ApiServerError> {
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    let router = build_router(state);

    let task = tokio::spawn(async move {
        if let Err(error) = axum::serve(listener, router).await {
            tracing::error!("api server stopped: {error}");
        }
    });

    Ok(ServerHandle { addr, task })
}

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

fn content_type_for_path(path: &str) -> &'static str {
    match path.rsplit('.').next().unwrap_or_default() {
        "html" => "text/html; charset=utf-8",
        "js" => "application/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "png" => "image/png",
        "svg" => "image/svg+xml",
        "ogg" => "audio/ogg",
        "txt" => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::croopor_api_test_support::build_test_state;
    use super::*;
    use std::fs;
    use tokio::sync::mpsc;

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

    #[tokio::test]
    async fn performance_rules_refresh_spawns_only_when_remote_url_is_configured() {
        let unset_root = croopor_api_test_support::test_root("app-refresh-unset");
        let unset_state = build_test_state(&unset_root, None);
        assert!(!spawn_performance_rules_refresh(&unset_state));
        let _ = fs::remove_dir_all(&unset_root);

        let configured_root = croopor_api_test_support::test_root("app-refresh-configured");
        let configured_state = build_test_state(
            &configured_root,
            Some("http://127.0.0.1:9/rules.json".to_string()),
        );
        assert!(spawn_performance_rules_refresh(&configured_state));
        let _ = fs::remove_dir_all(&configured_root);
    }

    #[tokio::test(start_paused = true)]
    async fn performance_rules_refresh_loop_runs_initially_and_after_interval() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let task = tokio::spawn(run_performance_rules_refresh_loop(
            move || {
                let tx = tx.clone();
                async move {
                    tx.send(()).expect("record refresh tick");
                }
            },
            Duration::from_secs(60),
        ));

        rx.recv().await.expect("initial refresh tick");
        tokio::task::yield_now().await;

        tokio::time::advance(Duration::from_secs(59)).await;
        tokio::task::yield_now().await;
        assert!(rx.try_recv().is_err());

        tokio::time::advance(Duration::from_secs(1)).await;
        rx.recv().await.expect("periodic refresh tick");
        task.abort();
    }
}

#[cfg(test)]
mod croopor_api_test_support {
    use crate::state::{AppState, AppStateInit, InstallStore, SessionStore};
    use croopor_config::{AppPaths, ConfigStore, InstanceStore};
    use croopor_performance::PerformanceManager;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;

    pub fn build_test_state(root: &Path, remote_rules_url: Option<String>) -> AppState {
        let paths = test_paths(root);
        let config = Arc::new(ConfigStore::load_from(paths.clone()).expect("load config"));
        let instances = Arc::new(InstanceStore::load_from(paths.clone()).expect("load instances"));
        AppState::new(AppStateInit {
            app_name: "Croopor".to_string(),
            version: "test".to_string(),
            config,
            instances,
            installs: Arc::new(InstallStore::new()),
            sessions: Arc::new(SessionStore::new()),
            performance: Arc::new(
                PerformanceManager::new_with_config_dir_and_remote_url(
                    &paths.config_dir,
                    remote_rules_url,
                )
                .expect("performance manager"),
            ),
            frontend_dir: root.join("frontend"),
        })
    }

    pub fn test_root(name: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "croopor-api-app-{name}-{}-{}",
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
}
