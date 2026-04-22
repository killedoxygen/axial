mod commands;
mod events;
mod state;

use croopor_api::app::spawn_background;
use croopor_api::state::{AppState, AppStateInit, InstallStore, SessionStore};
use croopor_config::{ConfigStore, InstanceStore};
use croopor_performance::PerformanceManager;
use std::sync::Arc;
use tauri::{Emitter, Manager};
use tracing::info;

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt::init();

    let config = Arc::new(ConfigStore::load_default().expect("load config store"));
    let instances = Arc::new(InstanceStore::load_default().expect("load instance store"));
    let installs = Arc::new(InstallStore::new());
    let sessions = Arc::new(SessionStore::new());
    let performance = Arc::new(PerformanceManager::new().expect("load performance manager"));
    let state = AppState::new(AppStateInit {
        app_name: "Croopor".to_string(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        config,
        instances,
        installs,
        sessions,
        performance,
        frontend_dir: croopor_api::app::default_frontend_dir(),
    });
    let desktop_state = state::DesktopState::new(env!("CARGO_PKG_VERSION").to_string());

    let api = spawn_background(state.clone())
        .await
        .expect("bind local croopor api server");

    info!("desktop shell connected to {}", api.addr);

    tauri::Builder::default()
        .manage(desktop_state)
        .manage(state)
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .invoke_handler(tauri::generate_handler![
            commands::app_version,
            commands::api_base_url,
            commands::start_install_events,
            commands::start_loader_install_events,
            commands::start_launch_events,
            commands::window_minimize,
            commands::window_toggle_maximize,
            commands::window_close,
            commands::window_is_maximized,
            commands::window_start_dragging
        ])
        .setup(move |app| {
            app.manage(state::ApiRuntimeState::new(api.addr));
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let _ = api.task.await;
                let _ = handle.emit(events::DESKTOP_API_STOPPED, serde_json::json!({}));
            });
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("run tauri desktop shell");
}
