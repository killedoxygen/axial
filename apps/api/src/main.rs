use axial_api::app::{DEFAULT_API_PORT, build_router};
use axial_api::bootstrap::{
    ApplicationLoadRequest, app_root_selection_from_environment, load_application,
};
use axial_api::observability::telemetry::{
    TelemetryErrorArea, TelemetryErrorKind, TelemetryErrorLevel, TelemetryEvent, TelemetryHub,
};
use axial_api::state::AppState;
use axial_api::transport::LocalApiAuthority;
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
    let addr = api_addr_from_environment()?;

    let loaded = load_application(ApplicationLoadRequest {
        root: app_root_selection_from_environment()?,
        app_name: "Axial".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
    })
    .await?;
    let state = loaded.state;
    tracing::debug!(health = ?loaded.health, "application startup settled");

    let telemetry = state.telemetry().clone();
    let result = serve_api(state, addr).await;
    if result.is_err() {
        emit_startup_failed(&telemetry);
    }
    result
}

fn api_addr_from_environment() -> Result<SocketAddr, Box<dyn std::error::Error>> {
    let addr = match std::env::var("AXIAL_API_ADDR") {
        Ok(value) => value.parse::<SocketAddr>()?,
        Err(std::env::VarError::NotPresent) => SocketAddr::from(([127, 0, 0, 1], DEFAULT_API_PORT)),
        Err(error) => return Err(error.into()),
    };
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AXIAL_API_ADDR must be a loopback address",
        )
        .into());
    }
    Ok(addr)
}

async fn serve_api(state: AppState, addr: SocketAddr) -> Result<(), Box<dyn std::error::Error>> {
    if !addr.ip().is_loopback() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "AXIAL_API_ADDR must be a loopback address",
        )
        .into());
    }
    let listener = match TcpListener::bind(addr).await {
        Ok(listener) => listener,
        Err(error) => return Err(listener_startup_error(&state, error).await),
    };
    let addr = match listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => return Err(listener_startup_error(&state, error).await),
    };
    let web_origin = match std::env::var("AXIAL_WEB_ORIGIN") {
        Ok(origin) => Some(origin),
        Err(std::env::VarError::NotPresent) => None,
        Err(error) => return Err(error.into()),
    };
    let authority = LocalApiAuthority::new(addr, web_origin.as_deref())
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidInput, error))?;
    info!("axial api listening on http://{addr}");

    let (stop_ingress, mut ingress_stopping) = tokio::sync::watch::channel(false);
    let server_state = state.clone();
    let mut server = std::pin::pin!(async move {
        axum::serve(listener, build_router(server_state, authority))
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
