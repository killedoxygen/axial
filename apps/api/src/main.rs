use croopor_api::app::{build_router, default_frontend_dir};
use croopor_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use croopor_config::{ConfigStore, InstanceStore};
use croopor_performance::PerformanceManager;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::net::TcpListener;
use tracing::info;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let config = Arc::new(ConfigStore::load_default()?);
    let instances = Arc::new(InstanceStore::load_default()?);
    let installs = Arc::new(InstallStore::new());
    let sessions = Arc::new(SessionStore::new());
    let performance = Arc::new(PerformanceManager::new()?);
    let state = AppState::new(AppStateInit {
        app_name: "Croopor".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        frontend_dir: default_frontend_dir(),
    });

    let addr = std::env::var("CROOPOR_API_ADDR")
        .ok()
        .and_then(|value| value.parse::<SocketAddr>().ok())
        .unwrap_or_else(|| SocketAddr::from(([127, 0, 0, 1], 43_430)));

    let listener = TcpListener::bind(addr).await?;
    info!("croopor api listening on http://{addr}");

    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await?;

    Ok(())
}
