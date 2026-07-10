use axial_api::app::{
    DEFAULT_API_PORT, build_router, default_frontend_dir, spawn_benchmark_suite_drivers_resume,
    spawn_performance_operations_resume, spawn_performance_rules_refresh,
    spawn_remote_flags_refresh, spawn_telemetry_export, spawn_update_staging_cleanup,
};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use axial_config::{AppPaths, ConfigStore, InstanceStore};
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

    let paths = AppPaths::detect();
    let config_startup = ConfigStore::load_for_startup(paths.clone())?;
    let instance_startup = InstanceStore::load_for_startup(paths.clone());
    let mut startup_warnings = config_startup.warnings;
    startup_warnings.extend(instance_startup.warnings);
    let config = Arc::new(config_startup.store);
    let instances = Arc::new(instance_startup.store);
    let installs = Arc::new(InstallStore::new());
    let sessions = Arc::new(SessionStore::new());
    let performance = Arc::new(PerformanceManager::new_with_config_dir(&paths.config_dir)?);
    let state = AppState::new(AppStateInit {
        app_name: "Axial".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        startup_warnings,
        frontend_dir: default_frontend_dir(),
    });
    spawn_performance_operations_resume(&state);
    spawn_benchmark_suite_drivers_resume(&state);
    spawn_performance_rules_refresh(&state);
    spawn_telemetry_export(&state);
    spawn_remote_flags_refresh(&state);
    spawn_update_staging_cleanup(&state);

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
    let listener = TcpListener::bind(addr).await?;
    let addr = listener.local_addr()?;
    info!("axial api listening on http://{addr}");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}

fn emit_startup_failed(telemetry: &Arc<TelemetryHub>) {
    telemetry.emit_sync_best_effort(TelemetryEvent::error_captured(
        TelemetryErrorKind::StartupFailed,
        TelemetryErrorArea::Startup,
        TelemetryErrorLevel::Error,
        "Backend startup failed.",
    ));
}
