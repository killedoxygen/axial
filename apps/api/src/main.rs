use axial_api::app::{DEFAULT_API_PORT, build_router, start_application_background_workflows};
use axial_api::bootstrap::{app_root_selection_from_environment, resolve_app_paths};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use axial_config::{ConfigStore, InstanceStore};
use axial_performance::PerformanceManager;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::runtime::Builder as TokioRuntimeBuilder;
use tracing::info;

const TOKIO_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    TokioRuntimeBuilder::new_multi_thread()
        .enable_all()
        .thread_stack_size(TOKIO_WORKER_STACK_BYTES)
        .build()?
        .block_on(run())
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let paths = resolve_app_paths(app_root_selection_from_environment()?)?;
    let config_paths = paths.clone();
    let config_startup =
        tokio::task::spawn_blocking(move || ConfigStore::load_for_startup(config_paths)).await??;
    let instance_paths = paths.clone();
    let instance_startup =
        tokio::task::spawn_blocking(move || InstanceStore::load_for_startup(instance_paths))
            .await?;
    let mut startup_warnings = config_startup.warnings;
    startup_warnings.extend(instance_startup.warnings);
    let config = Arc::new(config_startup.store);
    let instances = Arc::new(instance_startup.store);
    let installs = Arc::new(InstallStore::new());
    let sessions = Arc::new(SessionStore::new());
    let performance_dir = paths.performance_dir().to_path_buf();
    let performance = Arc::new(
        tokio::task::spawn_blocking(move || {
            PerformanceManager::load_for_startup(&performance_dir)
        })
        .await??,
    );
    let state = AppState::load(AppStateInit {
        app_name: "Axial".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        startup_warnings,
    })
    .await?;
    if !start_application_background_workflows(&state).await {
        return Err(std::io::Error::other("startup background ownership was refused").into());
    }

    let addr = std::env::var("AXIAL_API_ADDR")
        .ok()
        .and_then(|value| value.parse::<SocketAddr>().ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], DEFAULT_API_PORT)));

    let telemetry = state.telemetry().clone();
    let result = serve_api(state, addr).await;
    if result.is_err() {
        emit_startup_failed(&telemetry);
    }
    result
}

async fn serve_api(state: AppState, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => return Err(listener_startup_error(&state, error).await),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => return Err(listener_startup_error(&state, error).await),
    };
    info!("axial api listening on http://{addr}");

    let (stop_ingress, mut ingress_stopping) = tokio::sync::watch::channel(false);
    let server_state = state.clone();
    let mut server = std::pin::pin!(async move {
        axum::serve(listener, build_router(server_state))
            .with_graceful_shutdown(async move {
                while !*ingress_stopping.borrow_and_update() {
                    if ingress_stopping.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await
    });
    let shutdown_state = state.clone();
    let mut shutdown = std::pin::pin!(async move {
        let _ = tokio::signal::ctrl_c().await;
        stop_ingress.send_replace(true);
        shutdown_state.shutdown().await
    });

    tokio::select! {
        serve_result = &mut server => {
            let shutdown_result = state.shutdown().await;
            serve_result?;
            shutdown_result?;
        }
        shutdown_result = &mut shutdown => {
            let serve_result = server.await;
            serve_result?;
            shutdown_result?;
        }
    }
    Ok(())
}

async fn listener_startup_error(
    state: &AppState,
    error: std::io::Error,
) -> Box<dyn std::error::Error> {
    if let Err(shutdown_error) = state.shutdown().await {
        tracing::warn!(
            step = shutdown_error.step().as_str(),
            "application shutdown remained incomplete after API listener startup failed"
        );
    }
    Box::new(error)
}

fn emit_startup_failed(telemetry: &Arc<TelemetryHub>) {
    telemetry.emit_sync_best_effort(TelemetryEvent::error_captured(
        TelemetryErrorKind::StartupFailed,
        TelemetryErrorArea::Startup,
        TelemetryErrorLevel::Error,
        "Backend startup failed.",
    ));
}
